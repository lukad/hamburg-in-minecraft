//! ALKIS land use → per-tile surface-class rasters, and water-body labeling.

use std::collections::{BTreeMap, HashMap, VecDeque};

use anyhow::{Context, Result, bail};
use hhmc_core::Bbox;
use hhmc_core::geom::rasterize_polygon;
use hhmc_core::landcfg::{BlocksConfig, BodiesFile, BodyInfo, Lock, WaterConfig};
use hhmc_core::raster::{Grid, NODATA, read_f32, write_u8, write_u16};
use hhmc_core::vector::read_features;
use rayon::prelude::*;
use tracing::{debug, info, warn};

use crate::{Ctx, PREP_VERSION, covers, gdal, key_of, mark, mark_covering, up_to_date};

/// Version of the land-use and water labeling in this module; part of its prep key.
const WATER_PREP_VERSION: u32 = 4;

pub fn vectors(ctx: &Ctx) -> Result<()> {
    let src = ctx.sources.get("alkis_nutzung").context("no source alkis_nutzung")?;
    let raw = ctx.paths.raw().join("alkis_nutzung");
    let inputs: Vec<_> = src
        .files
        .iter()
        .map(|f| (raw.join(&f.path), f.sha256.clone().unwrap_or_default()))
        .collect();
    let out = ctx.paths.prepared().join("alkis_nutzung.fgb");
    let shas: Vec<&[u8]> = inputs.iter().map(|(_, s)| s.as_bytes()).collect();
    let key = key_of(&[&PREP_VERSION.to_le_bytes(), b"alkis_nutzung", &shas.concat()]);
    if up_to_date(&out, &key) {
        return Ok(());
    }
    std::fs::create_dir_all(ctx.paths.prepared())?;
    let _ = std::fs::remove_file(&out);
    for (i, (p, _)) in inputs.iter().enumerate() {
        // The WFS GeoJSON carries EPSG:25832 coordinates without a `crs` member, so the CRS
        // is assigned rather than transformed (GeoJSON would otherwise default to WGS84).
        let mut args = vec!["-f", "FlatGeobuf", "-nln", "nutzung", "-a_srs", "EPSG:25832"];
        if i > 0 {
            args.extend(["-update", "-append"]);
        }
        let (o, s) = (out.to_str().expect("utf-8"), p.to_str().expect("utf-8"));
        gdal::run("ogr2ogr", args.into_iter().chain([o, s]))?;
    }
    mark(&out, &key)?;
    info!("ALKIS land use converted to FlatGeobuf");
    Ok(())
}

fn area_bbox(ctx: &Ctx) -> Bbox {
    ctx.tiles
        .iter()
        .map(|t| t.read_bbox(&ctx.world.origin))
        .reduce(|a, b| a.union(&b))
        .expect("area has tiles")
}

/// Mosaics the prepared per-tile rasters of `id` over `bbox`.
pub(crate) fn mosaic(ctx: &Ctx, id: &str, bbox: &Bbox) -> Result<Grid<f32>> {
    let mut g = Grid::filled(bbox, NODATA);
    for t in &ctx.tiles {
        let tile = read_f32(&ctx.paths.prepared().join(id).join(format!("{}_{}.tif", t.rx, t.rz)))?;
        let c0 = (tile.min_e - g.min_e) as usize;
        let r0 = (g.max_n - tile.max_n) as usize;
        for r in 0..tile.h {
            let dst = (r0 + r) * g.w + c0;
            g.data[dst..dst + tile.w].copy_from_slice(&tile.data[r * tile.w..(r + 1) * tile.w]);
        }
    }
    Ok(g)
}

pub fn landuse_and_water(ctx: &Ctx) -> Result<()> {
    let blocks = BlocksConfig::load(&ctx.paths)?;
    let water = WaterConfig::load(&ctx.paths)?;
    // Inputs: the land-use vectors and the DEM mosaics, by their keys, and the configs.
    let p = ctx.paths.prepared();
    let mut parts = Vec::new();
    for f in [
        p.join("alkis_nutzung.fgb.key"),
        p.join("dgm1").join("mosaic.vrt.key"),
        p.join("dgmw").join("mosaic.vrt.key"),
        ctx.paths.config().join("blocks.toml"),
        ctx.paths.config().join("water.toml"),
    ] {
        parts.push(std::fs::read(&f).with_context(|| format!("reading {}", f.display()))?);
    }
    let structures = crate::barriers::extract(ctx)?;
    for f in &structures {
        parts.push(std::fs::read(f)?);
    }
    let refs: Vec<&[u8]> = parts.iter().map(Vec::as_slice).collect();
    let key = key_of(&[
        &PREP_VERSION.to_le_bytes(),
        &WATER_PREP_VERSION.to_le_bytes(),
        b"landuse_and_water",
        &refs.concat(),
    ]);
    let bodies_path = ctx.paths.prepared().join("water").join("bodies.json");
    if covers(&bodies_path, &key, &ctx.tiles) {
        return Ok(());
    }
    let bbox = area_bbox(ctx);
    let fgb = ctx.paths.prepared().join("alkis_nutzung.fgb");

    // Surface classes.
    let mut class = Grid::filled(&bbox, 0u8);
    let feats = read_features(&fgb, Some(&bbox), &["nutzart", "bez"])?;
    for f in &feats {
        let nutzart = f.props[0].as_deref().unwrap_or("");
        let c = blocks.classify(nutzart, f.props[1].as_deref());
        for poly in &f.polygons {
            rasterize_polygon(poly, class.min_e, class.max_n, class.w, class.h, |col, row| {
                class.set(col, row, c)
            });
        }
    }
    info!(polygons = feats.len(), "land use rasterized");

    // ALKIS as published (over bridges: the road); the infra stage takes deck surfaces
    // and the extent of bridges from it. Written before the grid is changed below.
    let out_alkis = ctx.paths.prepared().join("landuse_alkis");
    std::fs::create_dir_all(&out_alkis)?;
    ctx.tiles.par_iter().try_for_each(|t| {
        let b = t.read_bbox(&ctx.world.origin);
        write_u8(&out_alkis.join(format!("{}_{}.tif", t.rx, t.rz)), &class.window(&b)?, None)
    })?;
    let gaps = close_bridge_gaps(ctx, &blocks, &water, &mut class, &bbox)?;
    info!(cells = gaps, "cells under bridges reclassified as water");
    let mut locks = water.locks.clone();
    locks.extend(crate::barriers::disks(&structures, &water, &bbox)?);
    let (label, bodies) = label_water(ctx, &blocks, &water, &locks, &class, &bbox)?;

    let out_lu = ctx.paths.prepared().join("landuse");
    let out_w = ctx.paths.prepared().join("water");
    ctx.tiles.par_iter().try_for_each(|t| -> Result<()> {
        let b = t.read_bbox(&ctx.world.origin);
        write_u8(&out_lu.join(format!("{}_{}.tif", t.rx, t.rz)), &class.window(&b)?, None)?;
        write_u16(&out_w.join(format!("{}_{}.tif", t.rx, t.rz)), &label.window(&b)?, None)
    })?;
    std::fs::write(&bodies_path, serde_json::to_string_pretty(&BodiesFile { bodies })? + "\n")?;
    mark_covering(&bodies_path, &key, &ctx.tiles)?;
    Ok(())
}

/// Reclassifies non-water cells as water where DGM-W says the terrain is submerged and
/// ALKIS water is close: the water under bridges. Returns the number of cells changed.
fn close_bridge_gaps(
    ctx: &Ctx,
    blocks: &BlocksConfig,
    water: &WaterConfig,
    class: &mut Grid<u8>,
    bbox: &Bbox,
) -> Result<usize> {
    let dgmw = mosaic(ctx, "dgmw", bbox)?;
    let dgm1 = mosaic(ctx, "dgm1", bbox)?;
    let water_class =
        blocks.classes.iter().position(|c| c.water).context("blocks.toml has no water class")?
            as u8;
    let (w, h) = (class.w, class.h);
    let is_water: Vec<bool> = class.data.iter().map(|&c| blocks.is_water(c)).collect();
    // Distances fit u16: the gap is a few dozen meters (the grid spans the whole city).
    let max = water.gap_max_m.round().min(f64::from(u16::MAX - 1)) as u16;
    // Multi-source BFS from ALKIS water; each reached cell remembers the DGM1 height of
    // the water cell it grew from (the flattened water surface).
    let mut dist = vec![u16::MAX; w * h];
    let mut surface = vec![NODATA; w * h];
    let mut q: VecDeque<usize> = VecDeque::new();
    for i in 0..w * h {
        if is_water[i] {
            dist[i] = 0;
            surface[i] = dgm1.data[i];
            q.push_back(i);
        }
    }
    let mut changed = 0;
    while let Some(i) = q.pop_front() {
        if dist[i] >= max {
            continue;
        }
        for j in neighbors(w, h, i, &N4) {
            if dist[j] != u16::MAX || is_water[j] {
                continue;
            }
            let submerged =
                dgmw.data[j] != NODATA && f64::from(dgmw.data[j]) < water.gap_bed_max_nhn;
            let flat = dgmw.data[j] == NODATA
                && dgm1.data[j] != NODATA
                && surface[i] != NODATA
                && f64::from((dgm1.data[j] - surface[i]).abs()) <= water.gap_flat_tol_m;
            if submerged || flat {
                dist[j] = dist[i] + 1;
                surface[j] = surface[i];
                class.data[j] = water_class;
                changed += 1;
                q.push_back(j);
            }
        }
    }
    Ok(changed)
}

const N4: [(i64, i64); 4] = [(1, 0), (-1, 0), (0, 1), (0, -1)];
const N8: [(i64, i64); 8] = [(1, 0), (-1, 0), (0, 1), (0, -1), (1, 1), (1, -1), (-1, 1), (-1, -1)];

fn neighbors(
    w: usize,
    h: usize,
    i: usize,
    n: &'static [(i64, i64)],
) -> impl Iterator<Item = usize> {
    let (c, r) = ((i % w) as i64, (i / w) as i64);
    n.iter().filter_map(move |(dc, dr)| {
        let (nc, nr) = (c + dc, r + dr);
        (nc >= 0 && nr >= 0 && (nc as usize) < w && (nr as usize) < h)
            .then(|| nr as usize * w + nc as usize)
    })
}

/// Flood-fills `allowed` cells from `starts` with `id`; returns the cells filled.
fn fill(
    label: &mut [u16],
    allowed: &dyn Fn(usize) -> bool,
    w: usize,
    h: usize,
    starts: &[usize],
    id: u16,
    n: &'static [(i64, i64)],
) -> Vec<usize> {
    let mut q: VecDeque<usize> = VecDeque::new();
    let mut cells = Vec::new();
    for &s in starts {
        if label[s] == 0 && allowed(s) {
            label[s] = id;
            q.push_back(s);
        }
    }
    while let Some(i) = q.pop_front() {
        cells.push(i);
        for j in neighbors(w, h, i, n) {
            if label[j] == 0 && allowed(j) {
                label[j] = id;
                q.push_back(j);
            }
        }
    }
    cells
}

fn label_water(
    ctx: &Ctx,
    blocks: &BlocksConfig,
    water: &WaterConfig,
    locks: &[Lock],
    class: &Grid<u8>,
    bbox: &Bbox,
) -> Result<(Grid<u16>, Vec<BodyInfo>)> {
    let (w, h) = (class.w, class.h);
    let is_water: Vec<bool> = class.data.iter().map(|&c| blocks.is_water(c)).collect();
    // Lock disks, and the disks of twice their radius around them.
    let mut barrier = vec![false; w * h];
    let mut near = vec![false; w * h];
    for lock in locks {
        for (scale, mask) in [(1.0, &mut barrier), (2.0, &mut near)] {
            let r = lock.radius_m * scale;
            rasterize_polygon(
                &[(0..64)
                    .map(|k| {
                        let a = k as f64 / 64.0 * std::f64::consts::TAU;
                        (lock.center[0] + r * a.cos(), lock.center[1] + r * a.sin())
                    })
                    .collect()],
                class.min_e,
                class.max_n,
                w,
                h,
                |c, rr| mask[rr * w + c] = true,
            );
        }
    }

    let mut label = Grid { min_e: class.min_e, max_n: class.max_n, w, h, data: vec![0u16; w * h] };
    let mut bodies = Vec::new();
    let centroid = |cells: &[usize]| -> [f64; 2] {
        let n = cells.len().max(1) as f64;
        let (se, sn) = cells.iter().fold((0.0, 0.0), |(se, sn), &i| {
            (se + class.min_e + (i % w) as f64 + 0.5, sn + class.max_n - (i / w) as f64 - 0.5)
        });
        [(se / n).round(), (sn / n).round()]
    };

    // 1. Configured bodies from their seeds, with locks as barriers.
    for (bi, body) in water.bodies.iter().enumerate() {
        let id = u16::try_from(bi + 1)?;
        let mut starts = Vec::new();
        for s in &body.seeds {
            match class.cell_at(s[0], s[1]) {
                Some((c, r)) if barrier[r * w + c] => {
                    warn!(body = %body.id, seed = ?s, "seed lies on a lock or weir barrier; skipped")
                }
                Some((c, r)) if is_water[r * w + c] => starts.push(r * w + c),
                Some(_) => {
                    bail!("water.toml: seed {s:?} of `{}` is not on an ALKIS water cell", body.id)
                }
                None => warn!(body = %body.id, seed = ?s, "seed outside the area; skipped"),
            }
        }
        let allowed = |i: usize| is_water[i] && !barrier[i];
        let cells = fill(&mut label.data, &allowed, w, h, &starts, id, &N4);
        if !starts.is_empty() {
            info!(body = %body.id, cells = cells.len(), level = body.level_nhn, "water body labeled");
        }
        bodies.push(BodyInfo {
            index: id,
            id: body.id.clone(),
            level_nhn: body.level_nhn,
            level_source: "config".into(),
            cells: cells.len(),
            centroid: if cells.is_empty() { [0.0, 0.0] } else { centroid(&cells) },
        });
    }

    // 2. Lock cells, and water cut off next to a lock, take the label of the nearest
    //    labeled water (multi-source BFS limited to twice the lock radius).
    let mut q: VecDeque<usize> = (0..w * h).filter(|&i| label.data[i] != 0).collect();
    while let Some(i) = q.pop_front() {
        let id = label.data[i];
        for j in neighbors(w, h, i, &N8) {
            if label.data[j] == 0 && is_water[j] && (barrier[j] || near[j]) {
                label.data[j] = id;
                q.push_back(j);
            }
        }
    }

    // 3. Remaining components: level = median DGM1 of their shoreline cells. A component
    //    whose shoreline spans more than `stream_split_spread_m` (a stream running
    //    downhill) is split into bands of `stream_band_m` by the DGM1 of the nearest
    //    shoreline, each band a body of its own: the ground stage puts a gate where
    //    bands meet, so the stream steps down instead of standing above its lower banks.
    let dgm1 = mosaic(ctx, "dgm1", bbox)?;
    let mut next = water.bodies.len() + 1;
    let (mut split, mut pieces_total) = (0usize, 0usize);
    for i in 0..w * h {
        if !is_water[i] || label.data[i] != 0 {
            continue;
        }
        let Ok(id) = u16::try_from(next) else {
            bail!("more than {} water bodies in the area", u16::MAX)
        };
        let allowed = |j: usize| is_water[j];
        let cells = fill(&mut label.data, &allowed, w, h, &[i], id, &N4);
        let shore_of = |c: usize| {
            (neighbors(w, h, c, &N4).any(|n| !is_water[n]) && dgm1.data[c] != NODATA)
                .then(|| dgm1.data[c])
        };
        let mut shore: Vec<f32> = cells.iter().filter_map(|&c| shore_of(c)).collect();
        shore.sort_by(f32::total_cmp);
        // The level sits at the lower banks (a low quantile of the shoreline), not above
        // half of them as the median would.
        let q = (shore.len() as f64 * water.unseeded_level_quantile) as usize;
        let Some(&median) = shore.get(q.min(shore.len().saturating_sub(1))) else {
            warn!(cells = cells.len(), "water component without DGM1 shoreline; left dry");
            for c in cells {
                label.data[c] = 0;
            }
            continue;
        };
        let spread = shore[shore.len() * 9 / 10] - shore[shore.len() / 10];
        let pieces = if f64::from(spread) > water.stream_split_spread_m {
            split += 1;
            level_bands(&cells, &shore_of, median, w, h, water)
        } else {
            vec![(cells, median)]
        };
        pieces_total += pieces.len();
        for (k, (piece, level)) in pieces.into_iter().enumerate() {
            let Ok(id) = u16::try_from(next) else {
                bail!("more than {} water bodies in the area", u16::MAX)
            };
            for &c in &piece {
                label.data[c] = id;
            }
            let c = centroid(&piece);
            debug!(body = format!("auto-{next}"), cells = piece.len(), centroid = ?c, level, "unseeded water");
            bodies.push(BodyInfo {
                index: id,
                id: format!("auto-{next}"),
                level_nhn: f64::from(level),
                level_source: if k == 0 && spread <= water.stream_split_spread_m as f32 {
                    "median DGM1 along shoreline".into()
                } else {
                    "DGM1 of the nearest shoreline, in bands".into()
                },
                cells: piece.len(),
                centroid: c,
            });
            next += 1;
        }
    }
    info!(
        bodies = bodies.len() - water.bodies.len(),
        streams_split = split,
        pieces = pieces_total,
        "water components not reached from a configured seed: levels from their shorelines"
    );
    // 4. Flood land below a neighboring body's level (bathtub fill, capped).
    let flooded = flood_low_land(ctx, water, &is_water, &dgm1, &mut label, &bodies);
    info!(cells = flooded, "land cells below the adjacent water level flooded");
    Ok((label, bodies))
}

/// Splits a sloping water component into level bands: every cell takes the DGM1 of
/// its nearest shoreline cell (multi-source search within the component), rounded down to
/// `stream_band_m`; connected cells of one band form a piece, and pieces smaller than
/// `stream_min_piece_cells` join the lower band most common around them (a lowest speck
/// stays a piece). Returns the pieces
/// with their levels.
fn level_bands(
    cells: &[usize],
    shore_of: &dyn Fn(usize) -> Option<f32>,
    median: f32,
    w: usize,
    h: usize,
    water: &WaterConfig,
) -> Vec<(Vec<usize>, f32)> {
    let pos: HashMap<usize, usize> = cells.iter().enumerate().map(|(k, &c)| (c, k)).collect();
    let mut value: Vec<f32> = vec![f32::NAN; cells.len()];
    let mut q: VecDeque<usize> = VecDeque::new();
    for (k, &c) in cells.iter().enumerate() {
        if let Some(v) = shore_of(c) {
            value[k] = v;
            q.push_back(k);
        }
    }
    while let Some(k) = q.pop_front() {
        for n in neighbors(w, h, cells[k], &N4) {
            if let Some(&kn) = pos.get(&n)
                && value[kn].is_nan()
            {
                value[kn] = value[k];
                q.push_back(kn);
            }
        }
    }
    let step = water.stream_band_m as f32;
    let mut band: Vec<i32> = value
        .iter()
        .map(|&v| (if v.is_nan() { median } else { v } / step).floor() as i32)
        .collect();
    // Connected pieces of one band; small ones take their neighbors' most common band.
    let pieces_of = |band: &[i32]| -> Vec<Vec<usize>> {
        let mut seen = vec![false; cells.len()];
        let mut out = Vec::new();
        for s in 0..cells.len() {
            if seen[s] {
                continue;
            }
            seen[s] = true;
            let mut piece = vec![s];
            let mut i = 0;
            while i < piece.len() {
                let k = piece[i];
                i += 1;
                for n in neighbors(w, h, cells[k], &N4) {
                    if let Some(&kn) = pos.get(&n)
                        && !seen[kn]
                        && band[kn] == band[s]
                    {
                        seen[kn] = true;
                        piece.push(kn);
                    }
                }
            }
            out.push(piece);
        }
        out
    };
    for _ in 0..20 {
        let pieces = pieces_of(&band);
        let mut changed = false;
        for piece in pieces.iter().filter(|p| p.len() < water.stream_min_piece_cells) {
            let own = band[piece[0]];
            let mut count: BTreeMap<i32, usize> = BTreeMap::new();
            for &k in piece {
                for n in neighbors(w, h, cells[k], &N4) {
                    if let Some(&kn) = pos.get(&n)
                        && band[kn] != own
                    {
                        *count.entry(band[kn]).or_default() += 1;
                    }
                }
            }
            // Only down: raising a speck into a higher band would put water above its own
            // banks (and chains of specks on a slope climbed whole meters).
            if let Some((&b, _)) = count
                .iter()
                .filter(|&(&b, _)| b < own)
                .max_by_key(|&(b, n)| (*n, std::cmp::Reverse(*b)))
            {
                for &k in piece {
                    band[k] = b;
                }
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    pieces_of(&band)
        .into_iter()
        .map(|p| {
            let level = band[p[0]] as f32 * step;
            (p.into_iter().map(|k| cells[k]).collect(), level)
        })
        .collect()
}

fn flood_low_land(
    ctx: &Ctx,
    water: &WaterConfig,
    is_water: &[bool],
    dgm1: &Grid<f32>,
    label: &mut Grid<u16>,
    bodies: &[BodyInfo],
) -> usize {
    let (w, h) = (label.w, label.h);
    let label = &mut label.data;
    let hm = &ctx.world.height;
    let mut level_y = vec![i32::MIN; usize::from(u16::MAX) + 1];
    for b in bodies {
        level_y[b.index as usize] = hm.to_y(b.level_nhn).y;
    }
    let max = water.flood_max_m.round().min(f64::from(u16::MAX - 1)) as u16;
    let mut dist = vec![0u16; w * h];
    let mut q: VecDeque<usize> =
        (0..w * h).filter(|&i| label[i] != 0 && level_y[label[i] as usize] != i32::MIN).collect();
    let mut n = 0;
    while let Some(i) = q.pop_front() {
        if dist[i] >= max {
            continue;
        }
        let id = label[i];
        for j in neighbors(w, h, i, &N4) {
            if label[j] != 0 || is_water[j] || dgm1.data[j] == NODATA {
                continue;
            }
            if hm.to_y(f64::from(dgm1.data[j])).y < level_y[id as usize] {
                label[j] = id;
                dist[j] = dist[i] + 1;
                n += 1;
                q.push_back(j);
            }
        }
    }
    n
}
