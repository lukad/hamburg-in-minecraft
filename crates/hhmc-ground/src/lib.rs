//! Ground stage: for one tile and its read buffer, from prepared rasters,
//! - land: ground = round(DGM1);
//! - water: bed = DGM-W where present; elsewhere the body's level minus
//!   `default_depth_m`, blended into the nearest DGM-W bed over `blend_m`; water from the
//!   bed up to round(level of the cell's water body);
//! - surface class from the land-use raster.
//!
//! Land keeps DGM1 exactly (so the ground-height QA holds by construction); DGM1's
//! flattened water surfaces are never used as ground.

use std::collections::VecDeque;

use anyhow::{Result, ensure};
use hhmc_core::cache::KeyBuilder;
use hhmc_core::config::HeightMapping;
use hhmc_core::ground::{GroundModel, GroundStats, NO_WATER};
use hhmc_core::landcfg::{BodiesFile, WaterConfig};
use hhmc_core::raster::{NODATA, read_f32, read_u8, read_u16};
use hhmc_core::{Paths, TileKey};
use tracing::warn;

/// Bump when the logic below changes.
pub const GROUND_VERSION: u32 = 6;

pub struct GroundInputs<'a> {
    pub paths: &'a Paths,
    pub blocks: &'a hhmc_core::landcfg::BlocksConfig,
    pub water: &'a WaterConfig,
    pub bodies: &'a BodiesFile,
    pub height: &'a HeightMapping,
}

fn input(paths: &Paths, id: &str, t: TileKey) -> std::path::PathBuf {
    paths.prepared().join(id).join(format!("{}_{}.tif", t.rx, t.rz))
}

/// Cache key: version, configs, bodies and the four prepared rasters of the tile.
pub fn cache_key(paths: &Paths, t: TileKey) -> Result<[u8; 32]> {
    let mut k = KeyBuilder::new("ground", GROUND_VERSION);
    k.file(&paths.config().join("world.toml"))?
        .file(&paths.config().join("water.toml"))?
        .file(&paths.config().join("blocks.toml"))?
        .file(&paths.prepared().join("water/bodies.json"))?;
    for id in ["dgm1", "dgmw", "landuse", "water"] {
        k.file(&input(paths, id, t))?;
    }
    Ok(k.finish())
}

pub fn ground(inp: &GroundInputs, t: TileKey) -> Result<GroundModel> {
    let dgm1 = read_f32(&input(inp.paths, "dgm1", t))?;
    let dgmw = read_f32(&input(inp.paths, "dgmw", t))?;
    let class = read_u8(&input(inp.paths, "landuse", t))?;
    let body = read_u16(&input(inp.paths, "water", t))?;
    for g in [&dgmw.bbox(), &class.bbox(), &body.bbox()] {
        ensure!(*g == dgm1.bbox(), "tile {t}: prepared rasters are not aligned");
    }
    let (w, h) = (dgm1.w, dgm1.h);
    let levels = inp.bodies.levels();
    let level_of = |b: u16| levels.get(b as usize).copied().flatten();

    // Synthetic bed for water cells without DGM-W, blended into the nearest DGM-W bed.
    let bed = synthetic_beds(inp.water, &dgmw.data, &body.data, &level_of, w, h);

    let border = (w - hhmc_core::TILE_SIZE as usize) / 2;
    let inside = |i: usize| {
        let (c, r) = (i % w, i / w);
        (border..w - border).contains(&c) && (border..h - border).contains(&r)
    };
    let mut stats = GroundStats::default();
    let mut ground = vec![0i16; w * h];
    let mut water_top = vec![NO_WATER; w * h];
    let mut missing = Vec::new();
    for i in 0..w * h {
        let flooded_land =
            body.data[i] != 0 && !inp.blocks.is_water(class.data[i]) && dgm1.data[i] != NODATA;
        let height = match level_of(body.data[i]) {
            Some(level) if flooded_land => {
                // Land below the water level: its DGM1 height is the bed.
                let wy = inp.height.to_y(level).y;
                let g = map(inp.height, dgm1.data[i], inside(i), &mut stats);
                if wy > g {
                    water_top[i] = wy as i16;
                }
                ground[i] = g as i16;
                continue;
            }
            Some(level) => {
                let wy = inp.height.to_y(level).y;
                if bed[i].1 && inside(i) {
                    stats.synthetic_bed += 1;
                }
                let g = map(inp.height, bed[i].0, inside(i), &mut stats);
                if wy > g {
                    water_top[i] = wy as i16;
                }
                ground[i] = g as i16;
                continue;
            }
            None if dgm1.data[i] != NODATA => dgm1.data[i],
            None if dgmw.data[i] != NODATA => {
                if inside(i) {
                    stats.filled_land += 1;
                }
                dgmw.data[i]
            }
            None => {
                missing.push(i);
                continue;
            }
        };
        ground[i] = map(inp.height, height, inside(i), &mut stats) as i16;
    }
    // Lock gates: a water cell next to another body whose surface is lower (its water, or
    // its bed where it lies dry) becomes a gate column whose top is at its own water level,
    // so the higher water is held back.
    let gate_class = inp.blocks.class_index("lock_gate").ok();
    let mut class_data = class.data;
    let gates: Vec<usize> = (0..w * h)
        .filter(|&i| water_top[i] != NO_WATER)
        .filter(|&i| {
            let (c, r) = (i % w, i / w);
            [
                (c > 0).then(|| i - 1),
                (c + 1 < w).then(|| i + 1),
                (r > 0).then(|| i - w),
                (r + 1 < h).then(|| i + w),
            ]
            .into_iter()
            .flatten()
            .any(|j| {
                let surface = if water_top[j] != NO_WATER { water_top[j] } else { ground[j] };
                body.data[j] != 0 && body.data[j] != body.data[i] && surface < water_top[i]
            })
        })
        .collect();
    for &i in &gates {
        ground[i] = water_top[i];
        water_top[i] = NO_WATER;
        if let Some(g) = gate_class {
            class_data[i] = g;
        }
        if inside(i) {
            stats.lock_gates += 1;
        }
    }
    if !missing.is_empty() {
        fill_from_neighbors(&mut ground, &missing, w, h);
        stats.filled_land += missing.iter().filter(|&&i| inside(i)).count();
    }
    if stats.clamped > 0 || stats.filled_land > 0 {
        warn!(tile = %t, clamped = stats.clamped, filled_land = stats.filled_land, "ground heights outside data or range");
    }
    Ok(GroundModel {
        key: t,
        min_e: dgm1.min_e,
        max_n: dgm1.max_n,
        w,
        h,
        ground,
        class: class_data,
        water_top,
        body: body.data,
        stats,
    })
}

fn map(m: &HeightMapping, h: f32, inside: bool, stats: &mut GroundStats) -> i32 {
    let r = m.to_y(f64::from(h));
    if r.clamped && inside {
        stats.clamped += 1;
    }
    r.y
}

/// Per cell: (bed height, synthetic?). Non-water cells and cells with DGM-W get DGM-W.
fn synthetic_beds(
    water: &WaterConfig,
    dgmw: &[f32],
    body: &[u16],
    level_of: &dyn Fn(u16) -> Option<f64>,
    w: usize,
    h: usize,
) -> Vec<(f32, bool)> {
    let n = w * h;
    // BFS over water cells from water cells with DGM-W: distance and source bed.
    let mut dist = vec![u32::MAX; n];
    let mut src = vec![NODATA; n];
    let mut q = VecDeque::new();
    for i in 0..n {
        if body[i] != 0 && dgmw[i] != NODATA {
            dist[i] = 0;
            src[i] = dgmw[i];
            q.push_back(i);
        }
    }
    let max = water.blend_m.ceil() as u32;
    while let Some(i) = q.pop_front() {
        if dist[i] >= max {
            continue;
        }
        let (c, r) = (i % w, i / w);
        let nb = [
            (c > 0).then(|| i - 1),
            (c + 1 < w).then(|| i + 1),
            (r > 0).then(|| i - w),
            (r + 1 < h).then(|| i + w),
        ];
        for j in nb.into_iter().flatten() {
            if body[j] != 0 && dist[j] == u32::MAX {
                dist[j] = dist[i] + 1;
                src[j] = src[i];
                q.push_back(j);
            }
        }
    }
    (0..n)
        .map(|i| {
            if dgmw[i] != NODATA || body[i] == 0 {
                return (dgmw[i], false);
            }
            let target = level_of(body[i]).unwrap_or(0.0) - water.default_depth_m;
            if dist[i] == u32::MAX {
                (target as f32, true)
            } else {
                let t = (f64::from(dist[i]) / water.blend_m).min(1.0);
                ((f64::from(src[i]) + (target - f64::from(src[i])) * t) as f32, true)
            }
        })
        .collect()
}

/// Gives cells without any height the value of the nearest cell that has one.
fn fill_from_neighbors(ground: &mut [i16], missing: &[usize], w: usize, h: usize) {
    let mut known = vec![true; w * h];
    for &i in missing {
        known[i] = false;
    }
    let mut q: VecDeque<usize> = (0..w * h).filter(|&i| known[i]).collect();
    while let Some(i) = q.pop_front() {
        let (c, r) = (i % w, i / w);
        let nb = [
            (c > 0).then(|| i - 1),
            (c + 1 < w).then(|| i + 1),
            (r > 0).then(|| i - w),
            (r + 1 < h).then(|| i + w),
        ];
        for j in nb.into_iter().flatten() {
            if !known[j] {
                known[j] = true;
                ground[j] = ground[i];
                q.push_back(j);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hhmc_core::landcfg::Lock;

    fn wcfg() -> WaterConfig {
        WaterConfig {
            default_depth_m: 3.0,
            blend_m: 5.0,
            gap_bed_max_nhn: 0.5,
            gap_flat_tol_m: 0.02,
            gap_max_m: 40.0,
            flood_max_m: 30.0,
            stream_split_spread_m: 1.5,
            unseeded_level_quantile: 0.1,
            stream_band_m: 1.0,
            stream_min_piece_cells: 200,
            bodies: vec![],
            locks: Vec::<Lock>::new(),
            osm_barriers: Default::default(),
        }
    }

    #[test]
    fn blend_from_dgmw_to_synthetic() {
        // A 1 × 12 canal: DGM-W bed −5 in the first two cells, none afterwards; level 2.
        let w = 12;
        let mut dgmw = vec![NODATA; w];
        dgmw[0] = -5.0;
        dgmw[1] = -5.0;
        let body = vec![1u16; w];
        let beds = synthetic_beds(&wcfg(), &dgmw, &body, &|_| Some(2.0), w, 1);
        assert_eq!(beds[1], (-5.0, false));
        // One step away: 1/5 of the way from −5 to the synthetic −1.
        assert!((beds[2].0 - (-4.2)).abs() < 1e-5 && beds[2].1);
        // Five or more steps away: fully synthetic (level − depth = −1).
        assert_eq!(beds[6], (-1.0, true));
        assert_eq!(beds[11], (-1.0, true));
    }

    #[test]
    fn fill_missing() {
        let mut g = vec![5, 0, 0, 7];
        fill_from_neighbors(&mut g, &[1, 2], 4, 1);
        assert_eq!(g, vec![5, 5, 7, 7]);
    }
}
