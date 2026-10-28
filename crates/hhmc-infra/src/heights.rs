//! Deck height surface of one deck kind (all its polygons together); see config/infra.toml
//! `[heights]`.

use std::collections::HashMap;

use hhmc_core::infra::Heights;

use crate::Cells;

/// Cells without a smoothed value are interpolated from cells with one within this
/// distance (the tile buffer, so both tiles at an edge see the same cells).
const IDW_RADIUS_M: f64 = 32.0;

pub(crate) struct SurfaceInputs<'a> {
    /// The kind's cells on the buffered grid, sorted.
    pub union: &'a [usize],
    /// bDOM per cell of the union, whether the cell is unobstructed, and whether its bDOM
    /// stands clear of the ground or water (preferred; all unobstructed cells where too few
    /// do).
    pub vals: &'a HashMap<usize, (f32, bool, bool)>,
    pub cells: &'a Cells,
    pub cfg: &'a Heights,
    /// Ground y of the streets a cell lands on (see `streets` in `infra`).
    pub streets: &'a dyn Fn(usize) -> Vec<i32>,
}

/// Height (m) per union cell, `None` where nothing is known within reach.
pub(crate) fn surface(inp: &SurfaceInputs) -> Vec<Option<f32>> {
    let (cells, cfg) = (inp.cells, inp.cfg);
    // 1. Low quantile of unobstructed bDOM.
    let measured: HashMap<usize, f32> = inp
        .union
        .iter()
        .filter_map(|&i| quantile_near(inp.vals, cells, i, cfg).map(|v| (i, v)))
        .collect();
    // 2. Median over the smoothing radius.
    let mut smooth: HashMap<usize, f32> = HashMap::new();
    for &i in inp.union {
        let mut v = window(cells, i, cfg.smooth_radius_m, |n| measured.get(&n).copied());
        if v.is_empty() {
            continue;
        }
        v.sort_by(f32::total_cmp);
        smooth.insert(i, v[v.len() / 2]);
    }
    // Gaps: inverse distance from the smoothed cells.
    let known: Vec<((i64, i64), f32)> =
        inp.union.iter().filter_map(|&i| smooth.get(&i).map(|&v| (cells.col_row(i), v))).collect();
    let base: Vec<Option<f32>> = inp
        .union
        .iter()
        .map(|&i| smooth.get(&i).copied().or_else(|| idw(&known, cells.col_row(i), IDW_RADIUS_M)))
        .collect();
    // 3. Street anchors: residual to the street height, blended out from the anchor.
    let base_of: HashMap<usize, f32> =
        inp.union.iter().zip(&base).filter_map(|(&i, b)| b.map(|b| (i, b))).collect();
    let mut residual: HashMap<usize, f32> = HashMap::new();
    for &i in inp.union {
        let Some(&b) = base_of.get(&i) else { continue };
        let Some(street) = (inp.streets)(i).into_iter().max() else { continue };
        if (f64::from(b) - f64::from(street)).abs() <= cfg.anchor_tolerance_m {
            residual.insert(i, street as f32 - b);
        }
    }
    let blend = cfg.anchor_blend_m;
    inp.union
        .iter()
        .zip(base)
        .map(|(&i, b)| {
            let b = b?;
            let (c0, r0) = cells.col_row(i);
            let k = blend.ceil() as i64;
            let (mut sw, mut s, mut dmin) = (0.0f64, 0.0f64, f64::MAX);
            for dr in -k..=k {
                for dc in -k..=k {
                    let d = ((dc * dc + dr * dr) as f64).sqrt();
                    if d > blend {
                        continue;
                    }
                    let Some(n) = cells.at(c0 + dc, r0 + dr) else { continue };
                    let Some(&r) = residual.get(&n) else { continue };
                    let w = 1.0 / d.max(1.0).powi(2);
                    sw += w;
                    s += w * f64::from(r);
                    dmin = dmin.min(d);
                }
            }
            if sw == 0.0 {
                return Some(b);
            }
            let fade = (1.0 - dmin / blend).clamp(0.0, 1.0);
            Some(b + (s / sw * fade) as f32)
        })
        .collect()
}

/// Values of `get` over the cells within radius `r` of cell `i`.
fn window(cells: &Cells, i: usize, r: f64, get: impl Fn(usize) -> Option<f32>) -> Vec<f32> {
    let (c0, r0) = cells.col_row(i);
    let k = r.ceil() as i64;
    let mut v = Vec::new();
    for dr in -k..=k {
        for dc in -k..=k {
            if ((dc * dc + dr * dr) as f64).sqrt() > r {
                continue;
            }
            if let Some(x) = cells.at(c0 + dc, r0 + dr).and_then(&get) {
                v.push(x);
            }
        }
    }
    v
}

/// The configured quantile of unobstructed bDOM around cell `i`, within the median
/// radius, widening up to the maximum radius until there are at least `min_samples` (or
/// all there are at the maximum radius). Of
/// the clear samples where they are the majority: the deck stands clear of the ground or
/// water there (the others see past or through it); otherwise the deck is at street level
/// and the clear ones are what stands on it.
fn quantile_near(
    vals: &HashMap<usize, (f32, bool, bool)>,
    cells: &Cells,
    i: usize,
    h: &Heights,
) -> Option<f32> {
    let mut r = h.median_radius_m;
    let (mut all, mut clear) = (Vec::new(), Vec::new());
    while r <= h.max_radius_m {
        all = window(cells, i, r, |n| vals.get(&n).filter(|x| x.1).map(|x| x.0));
        clear = window(cells, i, r, |n| vals.get(&n).filter(|x| x.1 && x.2).map(|x| x.0));
        if all.len() >= h.min_samples {
            break;
        }
        r *= 2.0;
    }
    let mut v =
        if clear.len() >= h.min_samples && clear.len() * 2 >= all.len() { clear } else { all };
    // Small decks (footbridges and culverts over ditches) never reach `min_samples`: they
    // take what there is rather than no height at all.
    if v.is_empty() {
        return None;
    }
    v.sort_by(f32::total_cmp);
    Some(v[((v.len() - 1) as f64 * h.quantile).round() as usize])
}

/// Inverse-distance (power 2) weighted value at `p` from `known` within `radius`.
fn idw(known: &[((i64, i64), f32)], p: (i64, i64), radius: f64) -> Option<f32> {
    let r2 = radius * radius;
    let (mut sw, mut s) = (0.0f64, 0.0f64);
    for &((c, r), v) in known {
        let d2 = ((c - p.0).pow(2) + (r - p.1).pow(2)) as f64;
        if d2 <= r2 {
            let w = 1.0 / d2.max(1.0);
            sw += w;
            s += w * f64::from(v);
        }
    }
    (sw > 0.0).then(|| (s / sw) as f32)
}
