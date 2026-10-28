//! Structures for the infra stage, from ALKIS NAS:
//! - decks: AX_BauwerkImVerkehrsbereich polygons of the deck kinds in config/infra.toml
//!   (bridges, Hochbahn, landing stages);
//! - piers: its Brückenpfeiler polygons, plus supports generated at the configured spacing
//!   along the tracks of decks that ask for them, where ALKIS records no pier nearby;
//! - tracks: AX_Gleis lines, densified, with the parts inside ALKIS tunnels removed;
//! - occluders: footprints of buildings and canopies over decks (bDOM shows their roofs).
//!
//! Output: `infra/<rx>_<rz>.json` ([`TileInfra`]: everything whose bounding box touches
//! the tile's read area) and `infra/summary.json`.

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::Result;
use hhmc_core::geom::Bbox;
use hhmc_core::infra::{Deck, InfraConfig, Pier, Rings, TileInfra, Track, contains};
use hhmc_core::{TILE_BUFFER, TileKey, tile};
use rayon::prelude::*;
use serde::Serialize;
use tracing::info;

use crate::nas::{NasFeature, NasGeom};
use crate::{Ctx, PREP_VERSION, covers, key_of, mark_covering};

/// Bump when this step's logic changes (part of its key, with [`PREP_VERSION`]).
const INFRA_PREP_VERSION: u32 = 3;

/// AX_SonstigesBauwerkOderSonstigeEinrichtung: Bauwerksfunktion "Überdachung".
const CANOPY_BWF: i64 = 1610;

/// A track end within this distance (m) of an ALKIS tunnel enters it.
const PORTAL_M: f64 = 5.0;

/// Track points are at most this far apart.
const TRACK_STEP_M: f64 = 0.5;

#[derive(Default, Serialize)]
struct Summary {
    decks: BTreeMap<String, usize>,
    piers: usize,
    supports: usize,
    tracks: usize,
    track_km: f64,
    track_km_in_tunnels: f64,
    occluders: usize,
    portal_ends: usize,
}

pub fn prepare(ctx: &Ctx) -> Result<()> {
    let cfg = InfraConfig::load(&ctx.paths)?;
    let out_dir = ctx.paths.prepared().join("infra");
    let summary_path = out_dir.join("summary.json");
    let mut parts: Vec<String> = Vec::new();
    if let Some(s) = ctx.sources.get("alkis_nas") {
        parts.extend(s.files.iter().map(|f| f.sha256.clone().unwrap_or_default()));
    }
    parts.push(std::fs::read_to_string(InfraConfig::path(&ctx.paths))?);
    let key = key_of(&[
        &PREP_VERSION.to_le_bytes(),
        &INFRA_PREP_VERSION.to_le_bytes(),
        b"infra",
        parts.join(";").as_bytes(),
    ]);
    if covers(&summary_path, &key, &ctx.tiles) {
        return Ok(());
    }
    std::fs::create_dir_all(&out_dir)?;
    let mut files: Vec<PathBuf> = std::fs::read_dir(ctx.paths.raw().join("alkis_nas"))
        .map(|rd| {
            rd.flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|x| x == "xml"))
                .collect()
        })
        .unwrap_or_default();
    files.sort();
    let feats = crate::nas::read(
        &files,
        &[
            "AX_BauwerkImVerkehrsbereich",
            "AX_Gleis",
            "AX_Gebaeude",
            "AX_SonstigesBauwerkOderSonstigeEinrichtung",
        ],
    )?;
    let polys = |f: &NasFeature| match &f.geom {
        NasGeom::Polygons(p) => p.clone(),
        _ => Vec::new(),
    };
    let bwf = |f: &NasFeature| f.int("bauwerksfunktion").unwrap_or(0);
    let mut sum = Summary::default();

    let mut decks: Vec<Deck> = Vec::new();
    let mut piers: Vec<Pier> = Vec::new();
    let mut tunnels: Vec<(Bbox, Rings)> = Vec::new();
    for f in feats.iter().filter(|f| f.class == "AX_BauwerkImVerkehrsbereich") {
        let p = polys(f);
        if p.is_empty() {
            continue;
        }
        let b = bwf(f);
        if let Some(kind) = cfg.deck_kind(b) {
            *sum.decks.entry(cfg.deck[kind].kind.clone()).or_default() += 1;
            decks.push(Deck { id: f.id.clone(), kind, polygons: p });
        } else if cfg.piers.bauwerksfunktion.contains(&b) {
            piers.push(Pier { id: f.id.clone(), generated: false, polygons: p });
        } else if cfg.rails.tunnel_bauwerksfunktion.contains(&b) {
            for r in p {
                if let Some(bb) = Bbox::from_points(r[0].iter().copied()) {
                    tunnels.push((bb, r));
                }
            }
        }
    }
    sum.piers = piers.len();

    let in_tunnel =
        |e: f64, n: f64| tunnels.iter().any(|(b, r)| b.contains(e, n) && contains(r, e, n));
    let mut tracks: Vec<Track> = Vec::new();
    for f in feats.iter().filter(|f| f.class == "AX_Gleis") {
        let NasGeom::Lines(lines) = &f.geom else { continue };
        for l in lines {
            let pts = densify(l);
            let mut run: Vec<(f64, f64)> = Vec::new();
            for w in pts.windows(2) {
                sum.track_km += (w[1].0 - w[0].0).hypot(w[1].1 - w[0].1) / 1000.0;
            }
            for &p in &pts {
                if in_tunnel(p.0, p.1) {
                    sum.track_km_in_tunnels += TRACK_STEP_M / 1000.0;
                    if run.len() >= 2 {
                        tracks.push(Track {
                            id: f.id.clone(),
                            line: std::mem::take(&mut run),
                            portal_start: false,
                            portal_end: false,
                        });
                    }
                    run.clear();
                } else {
                    run.push(p);
                }
            }
            if run.len() >= 2 {
                tracks.push(Track {
                    id: f.id.clone(),
                    line: run,
                    portal_start: false,
                    portal_end: false,
                });
            }
        }
    }
    // Track ends at tunnel portals.
    let near_tunnel = |p: (f64, f64)| {
        tunnels.iter().any(|(b, r)| {
            b.buffered(PORTAL_M).contains(p.0, p.1)
                && (contains(r, p.0, p.1) || ring_distance(r, p) <= PORTAL_M)
        })
    };
    for t in &mut tracks {
        t.portal_start = near_tunnel(t.line[0]);
        t.portal_end = near_tunnel(*t.line.last().expect("two points"));
        sum.portal_ends += usize::from(t.portal_start) + usize::from(t.portal_end);
    }
    sum.tracks = tracks.len();

    // Supports along the tracks of decks that ask for them.
    let mut generated: Vec<Pier> = Vec::new();
    let centroid = |p: &Pier| {
        let r = &p.polygons[0][0];
        let n = r.len().max(1) as f64;
        (r.iter().map(|q| q.0).sum::<f64>() / n, r.iter().map(|q| q.1).sum::<f64>() / n)
    };
    for d in &decks {
        let spacing = cfg.deck[d.kind].support_spacing_m;
        if spacing <= 0.0 {
            continue;
        }
        for rings in &d.polygons {
            let Some(bb) = Bbox::from_points(rings[0].iter().copied()) else { continue };
            let mut k = 0;
            for t in &tracks {
                if !Bbox::from_points(t.line.iter().copied()).is_some_and(|tb| tb.intersects(&bb)) {
                    continue;
                }
                let mut along = spacing / 2.0;
                let mut acc = 0.0;
                for i in 1..t.line.len() {
                    let (a, b) = (t.line[i - 1], t.line[i]);
                    let seg = (b.0 - a.0).hypot(b.1 - a.1);
                    acc += seg;
                    if acc < along {
                        continue;
                    }
                    along += spacing;
                    if !bb.contains(b.0, b.1) || !contains(rings, b.0, b.1) || seg <= 0.0 {
                        continue;
                    }
                    let near = piers.iter().chain(&generated).any(|p| {
                        let c = centroid(p);
                        (c.0 - b.0).hypot(c.1 - b.1) < spacing / 2.0
                    });
                    if near {
                        continue;
                    }
                    // Across the deck: supports just inside both edges.
                    let dir = ((b.0 - a.0) / seg, (b.1 - a.1) / seg);
                    let nrm = (-dir.1, dir.0);
                    for side in [1.0, -1.0] {
                        let ray = (nrm.0 * side, nrm.1 * side);
                        let Some(s) = ray_exit(rings, b, ray) else { continue };
                        let s = (s - 0.5).max(0.0);
                        let c = (b.0 + ray.0 * s, b.1 + ray.1 * s);
                        let sq = vec![
                            (c.0 - 0.5, c.1 - 0.5),
                            (c.0 + 0.5, c.1 - 0.5),
                            (c.0 + 0.5, c.1 + 0.5),
                            (c.0 - 0.5, c.1 + 0.5),
                            (c.0 - 0.5, c.1 - 0.5),
                        ];
                        generated.push(Pier {
                            id: format!("support:{}:{k}", d.id),
                            generated: true,
                            polygons: vec![vec![sq]],
                        });
                        k += 1;
                    }
                }
            }
        }
    }
    sum.supports = generated.len();
    piers.extend(generated);

    // Buildings above ground and canopies (Überdachung, 1610) over a deck's bounding box.
    let deck_boxes: Vec<Bbox> = decks
        .iter()
        .filter_map(|d| Bbox::from_points(d.polygons.iter().flat_map(|r| r[0].iter().copied())))
        .collect();
    let mut occluders: Vec<Rings> = Vec::new();
    let canopy = |f: &NasFeature| {
        f.class == "AX_SonstigesBauwerkOderSonstigeEinrichtung" && bwf(f) == CANOPY_BWF
    };
    for f in feats.iter().filter(|f| f.class == "AX_Gebaeude" || canopy(f)) {
        // 1200 "Unter der Erdoberfläche": not in bDOM (1400 "Aufgeständert" is).
        if f.int("lageZurErdoberflaeche") == Some(1200) {
            continue;
        }
        for r in polys(f) {
            let Some(b) = Bbox::from_points(r[0].iter().copied()) else { continue };
            if deck_boxes.iter().any(|d| d.intersects(&b)) {
                occluders.push(r);
            }
        }
    }
    sum.occluders = occluders.len();

    // Per-tile files: each structure goes to the area's tiles whose read box it touches.
    let origin = ctx.world.origin;
    let rings_bbox = |p: &[Rings]| Bbox::from_points(p.iter().flat_map(|r| r[0].iter().copied()));
    let mut bins: BTreeMap<TileKey, TileInfra> =
        ctx.tiles.iter().map(|&t| (t, TileInfra::default())).collect();
    let mut bin = |b: Option<Bbox>, add: &mut dyn FnMut(&mut TileInfra)| {
        let Some(b) = b else { return };
        for t in tile::tiles_covering(&origin, &b.buffered(TILE_BUFFER)) {
            if b.intersects(&t.read_bbox(&origin))
                && let Some(ti) = bins.get_mut(&t)
            {
                add(ti);
            }
        }
    };
    for d in &decks {
        bin(rings_bbox(&d.polygons), &mut |ti| ti.decks.push(d.clone()));
    }
    for p in &piers {
        bin(rings_bbox(&p.polygons), &mut |ti| ti.piers.push(p.clone()));
    }
    for tr in &tracks {
        bin(Bbox::from_points(tr.line.iter().copied()), &mut |ti| ti.tracks.push(tr.clone()));
    }
    for r in &occluders {
        bin(Bbox::from_points(r[0].iter().copied()), &mut |ti| ti.occluders.push(r.clone()));
    }
    bins.par_iter().try_for_each(|(t, ti)| -> Result<()> {
        std::fs::write(TileInfra::path(&ctx.paths, *t), serde_json::to_string(ti)? + "\n")?;
        Ok(())
    })?;
    std::fs::write(&summary_path, serde_json::to_string_pretty(&sum)? + "\n")?;
    mark_covering(&summary_path, &key, &ctx.tiles)?;
    info!(
        decks = ?sum.decks,
        piers = sum.piers,
        supports = sum.supports,
        tracks = sum.tracks,
        track_km = format!("{:.1}", sum.track_km),
        in_tunnels_km = format!("{:.1}", sum.track_km_in_tunnels),
        "infrastructure prepared"
    );
    Ok(())
}

/// The line with points at most [`TRACK_STEP_M`] apart.
fn densify(l: &[(f64, f64)]) -> Vec<(f64, f64)> {
    let mut out = Vec::new();
    for w in l.windows(2) {
        let (a, b) = (w[0], w[1]);
        let n = (((b.0 - a.0).hypot(b.1 - a.1) / TRACK_STEP_M).ceil() as usize).max(1);
        for k in 0..n {
            let t = k as f64 / n as f64;
            out.push((a.0 + (b.0 - a.0) * t, a.1 + (b.1 - a.1) * t));
        }
    }
    if let Some(&last) = l.last() {
        out.push(last);
    }
    out
}

/// Distance from `p` to the nearest edge of the rings.
fn ring_distance(rings: &Rings, p: (f64, f64)) -> f64 {
    let mut best = f64::MAX;
    for r in rings {
        for w in r.windows(2) {
            let (a, b) = (w[0], w[1]);
            let (dx, dy) = (b.0 - a.0, b.1 - a.1);
            let l2 = dx * dx + dy * dy;
            let t = if l2 > 0.0 {
                (((p.0 - a.0) * dx + (p.1 - a.1) * dy) / l2).clamp(0.0, 1.0)
            } else {
                0.0
            };
            best = best.min((p.0 - a.0 - t * dx).hypot(p.1 - a.1 - t * dy));
        }
    }
    best
}

/// Distance from `p` (inside the polygon) along the unit direction `d` to the nearest ring
/// crossing.
fn ray_exit(rings: &Rings, p: (f64, f64), d: (f64, f64)) -> Option<f64> {
    let mut best: Option<f64> = None;
    for r in rings {
        for w in r.windows(2) {
            let (a, b) = (w[0], w[1]);
            let e = (b.0 - a.0, b.1 - a.1);
            let den = d.0 * e.1 - d.1 * e.0;
            if den.abs() < 1e-12 {
                continue;
            }
            let ap = (a.0 - p.0, a.1 - p.1);
            let s = (ap.0 * e.1 - ap.1 * e.0) / den;
            let u = (ap.0 * d.1 - ap.1 * d.0) / den;
            if s > 0.0 && (0.0..=1.0).contains(&u) && best.is_none_or(|x| s < x) {
                best = Some(s);
            }
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ray_leaves_a_square() {
        let sq = vec![vec![(0.0, 0.0), (10.0, 0.0), (10.0, 4.0), (0.0, 4.0), (0.0, 0.0)]];
        assert_eq!(ray_exit(&sq, (5.0, 1.0), (0.0, 1.0)), Some(3.0));
        assert_eq!(ray_exit(&sq, (5.0, 1.0), (0.0, -1.0)), Some(1.0));
    }

    #[test]
    fn densified_points_are_close() {
        let d = densify(&[(0.0, 0.0), (2.0, 0.0)]);
        assert_eq!(d.len(), 5);
        assert!(d.windows(2).all(|w| (w[1].0 - w[0].0) <= TRACK_STEP_M + 1e-9));
    }
}
