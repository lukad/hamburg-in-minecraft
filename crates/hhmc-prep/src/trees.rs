//! Tree lists for the vegetation stage.
//!
//! - Street trees: the BUKEA cadaster (`trees`) and the HPA port cadaster (`trees_hpa`).
//!   Heights come from `Hoehe_aus_ALS` of the 3D cadaster (`trees3d`), joined by tree
//!   number when the two positions agree; other trees get the median ALS height of 3D
//!   cadaster trees of the same genus and crown diameter (or girth).
//! - Park and wood trees: maxima of the canopy height model (bDOM − DGM1) in the
//!   configured surface classes, highest first with crown-sized suppression. The leaf-off
//!   bDOM underestimates trees, so heights are mapped through a linear fit of ALS height
//!   on the canopy maximum over the joined street trees, and crowns use the cadaster's
//!   median crown/height ratio (`trees/calibration.json`).
//! - Conflicts: a trunk on a blocked surface class, in water or in a building footprint
//!   moves to the nearest free cell within `snap_m`, else the tree is logged and skipped.
//!
//! Every tree is decided once, by the tile its trunk lies in, so trees crossing tile edges
//! come out identical on both sides. Output: `trees/<rx>_<rz>.json` ([`TileTrees`]),
//! `trees/calibration.json`, `trees/summary.json`.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::BufReader;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use hhmc_core::TileKey;
use hhmc_core::geom::Bbox;
use hhmc_core::landcfg::BlocksConfig;
use hhmc_core::raster::{Grid, NODATA, read_u8, read_u16};
use hhmc_core::trees::{
    Conflict, HeightSource, TileTrees, Tree, TreeSource, VegetationConfig, genus_of,
};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::{Ctx, PREP_VERSION, covers, key_of, mark, mark_covering, up_to_date};

/// Version of the tree decisions in this module; part of the prep key.
const TREES_PREP_VERSION: u32 = 3;

/// A tree of the 3D cadaster.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Tree3d {
    number: String,
    genus: String,
    e: f64,
    n: f64,
    height: Option<f32>,
    crown: Option<f32>,
    girth: Option<f32>,
}

/// Cadaster tree before heights and conflicts.
struct RawTree {
    id: String,
    source: TreeSource,
    number: Option<String>,
    e: f64,
    n: f64,
    genus: String,
    crown: Option<f32>,
    girth: Option<f32>,
}

/// Fits written to `trees/calibration.json`.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Calibration {
    /// Street trees joined to an ALS height, and those with canopy data around them.
    pub joined: usize,
    pub canopy_pairs: usize,
    /// ALS height ≈ `canopy_a + canopy_b` × canopy maximum within the crown.
    pub canopy_a: f64,
    pub canopy_b: f64,
    pub canopy_r2: f64,
    /// Median and p10/p90 of ALS − canopy maximum (before the fit).
    pub canopy_diff_median: f64,
    pub canopy_diff_p10: f64,
    pub canopy_diff_p90: f64,
    /// Median crown diameter / ALS height over the 3D cadaster.
    pub crown_per_height: f64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct Summary {
    street: usize,
    port: usize,
    canopy: usize,
    snapped: usize,
    height_sources: BTreeMap<String, usize>,
    conflicts: BTreeMap<String, usize>,
}

/// Minimum samples for a median height bin.
const MIN_BIN: usize = 20;
/// Largest distance (m) between a cadaster tree and its 3D-cadaster namesake.
const JOIN_MAX_M: f64 = 3.0;

pub fn prepare(ctx: &Ctx) -> Result<()> {
    let veg = VegetationConfig::load(&ctx.paths)?;
    let blocks = BlocksConfig::load(&ctx.paths)?;
    let out_dir = ctx.paths.prepared().join("trees");
    let summary_path = out_dir.join("summary.json");
    let key = prep_key(ctx)?;
    if covers(&summary_path, &key, &ctx.tiles) {
        return Ok(());
    }
    std::fs::create_dir_all(&out_dir)?;
    let origin = ctx.world.origin;
    let area = area_bbox(ctx);

    let t3d = trees3d(ctx, &out_dir)?;
    let heights = HeightModel::new(&t3d);
    let by_number: HashMap<&str, Vec<&Tree3d>> = t3d.iter().fold(HashMap::new(), |mut m, t| {
        m.entry(t.number.as_str()).or_default().push(t);
        m
    });

    // Area grids.
    let class = mosaic_u8(ctx, "landuse", &area)?;
    let water = mosaic_int(ctx, "water", &area, read_u16)?;
    let bdom = crate::landuse::mosaic(ctx, "bdom1", &area)?;
    let dgm = crate::landuse::mosaic(ctx, "dgm1", &area)?;
    let footprint = footprints(ctx, &area)?;
    let class_ids = |names: &[String]| -> Result<Vec<u8>> {
        names
            .iter()
            .map(|n| {
                blocks
                    .classes
                    .iter()
                    .position(|c| &c.id == n)
                    .map(|i| i as u8)
                    .with_context(|| format!("vegetation.toml: unknown surface class {n:?}"))
            })
            .collect()
    };
    let blocked_classes = class_ids(&veg.conflicts.blocked_classes)?;
    let park_classes = class_ids(&veg.parks.classes)?;
    let near_building = dilate(&footprint, 1);
    let blocked = |c: usize, r: usize| -> Option<String> {
        if near_building.get(c, r) {
            Some("building".into())
        } else if water.get(c, r) != 0 {
            Some("water".into())
        } else if blocked_classes.contains(&class.get(c, r)) {
            Some(blocks.classes[class.get(c, r) as usize].id.clone())
        } else {
            None
        }
    };

    // Street trees: heights, then conflicts.
    let raw = cadaster(ctx, &area)?;
    let mut summary = Summary::default();
    let mut placed: Vec<Tree> = Vec::new();
    let mut conflicts: Vec<Conflict> = Vec::new();
    let mut als_pairs: Vec<(Tree, f32)> = Vec::new();
    for t in raw {
        let als = t.number.as_deref().and_then(|num| {
            by_number.get(num)?.iter().find_map(|c| {
                ((c.e - t.e).hypot(c.n - t.n) <= JOIN_MAX_M).then_some(c.height).flatten()
            })
        });
        let (h, src) = match als {
            Some(h) if h > 0.0 => (h, HeightSource::Als),
            _ => heights.estimate(&t.genus, t.crown, t.girth),
        };
        let h = h.clamp(veg.tree.min_height_m, veg.tree.max_height_m);
        let crown = t.crown.filter(|&c| c > 0.0).unwrap_or(veg.tree.default_crown_m);
        let Some((c, r)) = class.cell_at(t.e, t.n) else { continue };
        let (e, n, snapped) = match blocked(c, r) {
            None => (t.e, t.n, 0.0),
            Some(reason) => {
                match snap(c, r, veg.conflicts.snap_m, |c, r| blocked(c, r).is_none()) {
                    Some((c2, r2, d)) => {
                        let (e, n) = (class.min_e + c2 as f64 + 0.5, class.max_n - r2 as f64 - 0.5);
                        (e, n, d as f32)
                    }
                    None => {
                        *summary.conflicts.entry(reason.clone()).or_default() += 1;
                        conflicts.push(Conflict { id: t.id, e: t.e, n: t.n, reason });
                        continue;
                    }
                }
            }
        };
        match t.source {
            TreeSource::Street => summary.street += 1,
            TreeSource::Port => summary.port += 1,
            TreeSource::Canopy => {}
        }
        summary.snapped += usize::from(snapped > 0.0);
        *summary.height_sources.entry(format!("{src:?}").to_lowercase()).or_default() += 1;
        let tree = Tree {
            id: t.id,
            source: t.source,
            e,
            n,
            genus: t.genus,
            height_m: h,
            height_source: src,
            crown_m: crown,
            girth_cm: t.girth,
            snapped_m: snapped,
        };
        if src == HeightSource::Als {
            als_pairs.push((tree.clone(), h));
        }
        placed.push(tree);
    }

    // Canopy calibration on the joined street trees.
    let chm = |c: usize, r: usize| -> Option<f32> {
        let (b, g) = (bdom.get(c, r), dgm.get(c, r));
        (b != NODATA && g != NODATA && !near_building.get(c, r)).then(|| (b - g).max(0.0))
    };
    let mut cal = Calibration { joined: als_pairs.len(), ..Default::default() };
    let pairs: Vec<(f64, f64)> = als_pairs
        .iter()
        .filter_map(|(t, h)| {
            let (c0, r0) = class.cell_at(t.e, t.n)?;
            let rad = (f64::from(t.crown_m) / 2.0).max(1.5);
            let k = rad.ceil() as i64;
            let mut best: Option<f32> = None;
            for dr in -k..=k {
                for dc in -k..=k {
                    if ((dc * dc + dr * dr) as f64).sqrt() > rad {
                        continue;
                    }
                    let (c, r) = (c0 as i64 + dc, r0 as i64 + dr);
                    if c < 0 || r < 0 || c as usize >= class.w || r as usize >= class.h {
                        continue;
                    }
                    if let Some(v) = chm(c as usize, r as usize) {
                        best = Some(best.map_or(v, |b| b.max(v)));
                    }
                }
            }
            best.map(|b| (f64::from(b), f64::from(*h)))
        })
        .collect();
    cal.canopy_pairs = pairs.len();
    let mut diffs: Vec<f64> = pairs.iter().map(|(x, y)| y - x).collect();
    diffs.sort_by(f64::total_cmp);
    if pairs.len() >= 30 {
        let q = |p: f64| diffs[((diffs.len() - 1) as f64 * p).round() as usize];
        (cal.canopy_diff_p10, cal.canopy_diff_median, cal.canopy_diff_p90) =
            (q(0.1), q(0.5), q(0.9));
        (cal.canopy_a, cal.canopy_b, cal.canopy_r2) = linear_fit(&pairs);
    } else {
        warn!(pairs = pairs.len(), "too few joined trees for the canopy fit, using CHM as is");
        (cal.canopy_a, cal.canopy_b) = (0.0, 1.0);
    }
    cal.crown_per_height = heights.crown_per_height;
    info!(
        joined = cal.joined,
        pairs = cal.canopy_pairs,
        a = format!("{:.2}", cal.canopy_a),
        b = format!("{:.3}", cal.canopy_b),
        r2 = format!("{:.2}", cal.canopy_r2),
        crown_per_height = format!("{:.3}", cal.crown_per_height),
        "tree height calibration"
    );

    // Park and wood trees, decided per tile on the tile's buffered window.
    let mut canopy: Vec<Tree> = Vec::new();
    if veg.parks.enabled {
        // bDOM also sees bridge decks and canopy roofs (infra structures) above the ground.
        let mut solid = footprint.clone();
        structures(ctx, &mut solid)?;
        let clearance = dilate(&solid, veg.parks.building_clearance_m.ceil() as usize);
        let crown_of = |h: f32| {
            (h * cal.crown_per_height as f32).clamp(veg.parks.min_crown_m, veg.parks.max_crown_m)
        };
        let street_index = PointIndex::new(&placed);
        let per_tile: Vec<Vec<Tree>> = ctx
            .tiles
            .par_iter()
            .map(|t| {
                let rb = t.read_bbox(&origin);
                let (c0, r0) = class.cell_at(rb.min_e + 0.5, rb.max_n - 0.5).expect("in area");
                let (w, h) = (rb.width().round() as usize, rb.height().round() as usize);
                let mut cand: Vec<(f32, usize, usize)> = Vec::new();
                for r in r0..r0 + h {
                    for c in c0..c0 + w {
                        if !park_classes.contains(&class.get(c, r))
                            || water.get(c, r) != 0
                            || clearance.get(c, r)
                        {
                            continue;
                        }
                        let Some(v) = chm(c, r) else { continue };
                        if v < veg.parks.min_height_m {
                            continue;
                        }
                        let (e, n) = (class.min_e + c as f64 + 0.5, class.max_n - r as f64 - 0.5);
                        if street_index.in_crown(e, n) {
                            continue;
                        }
                        cand.push((v, c, r));
                    }
                }
                // Highest first; ties in a fixed spatial order.
                cand.sort_by(|a, b| b.0.total_cmp(&a.0).then((a.2, a.1).cmp(&(b.2, b.1))));
                let mut accepted: Vec<(f64, f64, f32, f32)> = Vec::new();
                let mut grid: HashMap<(i64, i64), Vec<usize>> = HashMap::new();
                const CELL: f64 = 16.0;
                let mut out = Vec::new();
                for (v, c, r) in cand {
                    let (e, n) = (class.min_e + c as f64 + 0.5, class.max_n - r as f64 - 0.5);
                    let hgt = (cal.canopy_a + cal.canopy_b * f64::from(v)) as f32;
                    let hgt = hgt.max(v).clamp(veg.tree.min_height_m, veg.tree.max_height_m);
                    let crown = crown_of(hgt);
                    let (gx, gy) = ((e / CELL).floor() as i64, (n / CELL).floor() as i64);
                    let mut free = true;
                    'near: for dy in -1..=1 {
                        for dx in -1..=1 {
                            for &i in grid.get(&(gx + dx, gy + dy)).map_or(&[][..], |v| v) {
                                let (ae, an, _, acrown) = accepted[i];
                                let rad =
                                    f64::from(veg.parks.suppress_factor * acrown.max(crown)) / 2.0;
                                if (ae - e).hypot(an - n) < rad {
                                    free = false;
                                    break 'near;
                                }
                            }
                        }
                    }
                    if !free {
                        continue;
                    }
                    grid.entry((gx, gy)).or_default().push(accepted.len());
                    accepted.push((e, n, hgt, crown));
                    if TileKey::from_utm(&origin, e, n) == *t {
                        out.push(Tree {
                            id: format!("canopy:{}_{}", e.floor() as i64, n.floor() as i64),
                            source: TreeSource::Canopy,
                            e,
                            n,
                            genus: String::new(),
                            height_m: hgt,
                            height_source: HeightSource::Canopy,
                            crown_m: crown,
                            girth_cm: None,
                            snapped_m: 0.0,
                        });
                    }
                }
                out
            })
            .collect();
        canopy = per_tile.into_iter().flatten().collect();
        summary.canopy = canopy.len();
        *summary.height_sources.entry("canopy".into()).or_default() += canopy.len();
    }
    placed.extend(canopy);

    // Per-tile files.
    // A tree belongs to its tile and is "near" the tiles its crown (plus 2 m) reaches.
    let tile_of = |e: f64, n: f64| TileKey::from_utm(&origin, e, n);
    let mut bins: BTreeMap<TileKey, TileTrees> =
        ctx.tiles.iter().map(|&t| (t, TileTrees::default())).collect();
    for tr in &placed {
        let own = tile_of(tr.e, tr.n);
        let d = f64::from(tr.crown_m) / 2.0 + 2.0;
        let reach = Bbox::new(tr.e - d, tr.n - d, tr.e + d, tr.n + d);
        for t in hhmc_core::tile::tiles_covering(&origin, &reach) {
            let Some(tt) = bins.get_mut(&t) else { continue };
            if t == own {
                tt.own.push(tr.clone());
            } else if t.bbox(&origin).buffered(d).contains(tr.e, tr.n) {
                tt.near.push(tr.clone());
            }
        }
    }
    for c in &conflicts {
        if let Some(tt) = bins.get_mut(&tile_of(c.e, c.n)) {
            tt.conflicts.push(c.clone());
        }
    }
    bins.par_iter().try_for_each(|(t, tt)| -> Result<()> {
        std::fs::write(TileTrees::path(&ctx.paths, *t), serde_json::to_string(tt)? + "\n")?;
        Ok(())
    })?;
    std::fs::write(out_dir.join("calibration.json"), serde_json::to_string_pretty(&cal)? + "\n")?;
    std::fs::write(&summary_path, serde_json::to_string_pretty(&summary)? + "\n")?;
    mark_covering(&summary_path, &key, &ctx.tiles)?;
    info!(
        street = summary.street,
        port = summary.port,
        canopy = summary.canopy,
        snapped = summary.snapped,
        conflicts = conflicts.len(),
        "trees prepared"
    );
    Ok(())
}

fn prep_key(ctx: &Ctx) -> Result<String> {
    let mut parts: Vec<String> = Vec::new();
    for id in ["trees", "trees_hpa", "trees3d"] {
        if let Some(s) = ctx.sources.get(id) {
            parts.extend(s.files.iter().map(|f| f.sha256.clone().unwrap_or_default()));
        }
    }
    for f in ["vegetation.toml", "blocks.toml"] {
        parts.push(std::fs::read_to_string(ctx.paths.config().join(f))?);
    }
    // Upstream prep outputs, by their own keys (area-wide ones, so that the key does not
    // depend on the tiles; see `covers`).
    let p = ctx.paths.prepared();
    let sidecars = [
        p.join("buildings").join("index.json.key"),
        p.join("infra").join("summary.json.key"),
        p.join("water").join("bodies.json.key"),
        p.join("bdom1").join("mosaic.vrt.key"),
        p.join("dgm1").join("mosaic.vrt.key"),
    ];
    for s in sidecars {
        // Outputs without a sidecar are hashed directly.
        parts.push(match std::fs::read_to_string(&s) {
            Ok(k) => k,
            Err(_) => {
                let f = s.with_extension("");
                key_of(&[&std::fs::read(&f).with_context(|| format!("reading {}", f.display()))?])
            }
        });
    }
    Ok(key_of(&[
        &PREP_VERSION.to_le_bytes(),
        &TREES_PREP_VERSION.to_le_bytes(),
        b"trees",
        parts.join(";").as_bytes(),
    ]))
}

fn area_bbox(ctx: &Ctx) -> Bbox {
    ctx.tiles
        .iter()
        .map(|t| t.read_bbox(&ctx.world.origin))
        .reduce(|a, b| a.union(&b))
        .expect("area has tiles")
}

fn mosaic_u8(ctx: &Ctx, id: &str, bbox: &Bbox) -> Result<Grid<u8>> {
    mosaic_int(ctx, id, bbox, read_u8)
}

/// Mosaics the per-tile integer rasters of `id` (water body ids are 16-bit).
fn mosaic_int<T: Copy + Default>(
    ctx: &Ctx,
    id: &str,
    bbox: &Bbox,
    read: fn(&Path) -> Result<Grid<T>>,
) -> Result<Grid<T>> {
    let mut g = Grid::filled(bbox, T::default());
    for t in &ctx.tiles {
        let tile = read(&ctx.paths.prepared().join(id).join(format!("{}_{}.tif", t.rx, t.rz)))?;
        let (c0, r0) =
            g.cell_at(tile.min_e + 0.5, tile.max_n - 0.5).context("tile outside area")?;
        for r in 0..tile.h {
            for c in 0..tile.w {
                g.set(c0 + c, r0 + r, tile.get(c, r));
            }
        }
    }
    Ok(g)
}

/// Cells of the area's building columns and surface voxels, as the buildings stage
/// voxelizes them (walls, eaves, overhangs and open roofs included).
fn footprints(ctx: &Ctx, area: &Bbox) -> Result<Grid<bool>> {
    let mut g = Grid::filled(area, false);
    let mut seen: HashSet<String> = HashSet::new();
    let mut meshes = Vec::new();
    for t in &ctx.tiles {
        let bin = ctx.paths.prepared().join("buildings").join(format!("{}_{}.bin", t.rx, t.rz));
        if !bin.exists() {
            continue;
        }
        meshes.extend(
            hhmc_buildings::load_meshes(&bin)?.into_iter().filter(|m| seen.insert(m.id.clone())),
        );
    }
    let origin = ctx.world.origin;
    let cols: Vec<Vec<(i32, i32)>> = meshes
        .par_iter()
        .map(|m| {
            let v = hhmc_buildings::voxelize(m, &origin);
            // Volume columns, and surface voxels (installations, overhangs, open roofs).
            let mut c: Vec<(i32, i32)> = v.columns.into_keys().collect();
            c.extend(v.surface.iter().map(|&(x, _, z)| (x, z)));
            c
        })
        .collect();
    for (x, z) in cols.into_iter().flatten() {
        let (e, n) = origin.block_center(x, z);
        if let Some((c, r)) = g.cell_at(e, n) {
            g.set(c, r, true);
        }
    }
    Ok(g)
}

/// Adds the cells of the prepared decks and canopy roofs (`data/prepared/infra/`) of the
/// area's tiles to `g`.
fn structures(ctx: &Ctx, g: &mut Grid<bool>) -> Result<()> {
    let mut seen: HashSet<String> = HashSet::new();
    let (w, h, min_e, max_n) = (g.w, g.h, g.min_e, g.max_n);
    for t in &ctx.tiles {
        let p = hhmc_core::infra::TileInfra::path(&ctx.paths, *t);
        if !p.exists() {
            continue;
        }
        let ti = hhmc_core::infra::TileInfra::load(&p)?;
        let decks =
            ti.decks.iter().filter(|d| seen.insert(d.id.clone())).flat_map(|d| d.polygons.iter());
        for rings in decks.chain(ti.occluders.iter()) {
            hhmc_core::geom::rasterize_polygon(rings, min_e, max_n, w, h, |c, r| g.set(c, r, true));
        }
    }
    Ok(())
}

/// Square dilation by `k` cells.
fn dilate(g: &Grid<bool>, k: usize) -> Grid<bool> {
    let mut rows = g.clone();
    for r in 0..g.h {
        for c in 0..g.w {
            let (a, b) = (c.saturating_sub(k), (c + k).min(g.w - 1));
            rows.set(c, r, (a..=b).any(|cc| g.get(cc, r)));
        }
    }
    let mut out = rows.clone();
    for r in 0..g.h {
        let (a, b) = (r.saturating_sub(k), (r + k).min(g.h - 1));
        for c in 0..g.w {
            out.set(c, r, (a..=b).any(|rr| rows.get(c, rr)));
        }
    }
    out
}

/// Nearest cell within `max_m` (by center distance, then row, then column) that `ok`
/// accepts.
fn snap(
    c: usize,
    r: usize,
    max_m: f64,
    ok: impl Fn(usize, usize) -> bool,
) -> Option<(usize, usize, f64)> {
    let k = max_m.floor() as i64;
    let mut best: Option<(f64, i64, i64)> = None;
    for dr in -k..=k {
        for dc in -k..=k {
            let d = ((dc * dc + dr * dr) as f64).sqrt();
            if d == 0.0 || d > max_m {
                continue;
            }
            let (cc, rr) = (c as i64 + dc, r as i64 + dr);
            if cc < 0 || rr < 0 || !ok(cc as usize, rr as usize) {
                continue;
            }
            if best.is_none_or(|b| (d, rr, cc) < b) {
                best = Some((d, rr, cc));
            }
        }
    }
    best.map(|(d, rr, cc)| (cc as usize, rr as usize, d))
}

/// Least squares `y = a + b x`; returns (a, b, r²).
fn linear_fit(p: &[(f64, f64)]) -> (f64, f64, f64) {
    let n = p.len() as f64;
    let (mx, my) = (p.iter().map(|q| q.0).sum::<f64>() / n, p.iter().map(|q| q.1).sum::<f64>() / n);
    let sxy: f64 = p.iter().map(|q| (q.0 - mx) * (q.1 - my)).sum();
    let sxx: f64 = p.iter().map(|q| (q.0 - mx).powi(2)).sum();
    let syy: f64 = p.iter().map(|q| (q.1 - my).powi(2)).sum();
    let b = if sxx > 0.0 { sxy / sxx } else { 1.0 };
    let r2 = if sxx > 0.0 && syy > 0.0 { sxy * sxy / (sxx * syy) } else { 0.0 };
    (my - b * mx, b, r2)
}

/// Street trees for "is this point inside a street tree's crown".
struct PointIndex<'a> {
    cells: HashMap<(i64, i64), Vec<&'a Tree>>,
}

impl<'a> PointIndex<'a> {
    const CELL: f64 = 16.0;

    fn new(trees: &'a [Tree]) -> Self {
        let mut cells: HashMap<(i64, i64), Vec<&Tree>> = HashMap::new();
        for t in trees {
            let k = ((t.e / Self::CELL).floor() as i64, (t.n / Self::CELL).floor() as i64);
            cells.entry(k).or_default().push(t);
        }
        Self { cells }
    }

    fn in_crown(&self, e: f64, n: f64) -> bool {
        let (gx, gy) = ((e / Self::CELL).floor() as i64, (n / Self::CELL).floor() as i64);
        (-1..=1).any(|dy| {
            (-1..=1).any(|dx| {
                self.cells.get(&(gx + dx, gy + dy)).is_some_and(|v| {
                    v.iter().any(|t| (t.e - e).hypot(t.n - n) <= f64::from(t.crown_m) / 2.0)
                })
            })
        })
    }
}

/// Median ALS heights of the 3D cadaster by genus and crown diameter (1 m bins) or girth
/// (25 cm bins), with all-genus fallbacks.
struct HeightModel {
    crown: HashMap<(String, i32), f32>,
    girth: HashMap<(String, i32), f32>,
    genus: HashMap<String, f32>,
    crown_per_height: f64,
}

impl HeightModel {
    fn new(t3d: &[Tree3d]) -> Self {
        let mut crown: HashMap<(String, i32), Vec<f32>> = HashMap::new();
        let mut girth: HashMap<(String, i32), Vec<f32>> = HashMap::new();
        let mut genus: HashMap<String, Vec<f32>> = HashMap::new();
        let mut ratio: Vec<f64> = Vec::new();
        for t in t3d {
            let Some(h) = t.height.filter(|&h| h > 0.0) else { continue };
            for g in [t.genus.clone(), String::new()] {
                if let Some(c) = t.crown.filter(|&c| c > 0.0) {
                    crown.entry((g.clone(), c.round() as i32)).or_default().push(h);
                }
                if let Some(u) = t.girth.filter(|&u| u > 0.0) {
                    girth.entry((g.clone(), (u / 25.0).round() as i32)).or_default().push(h);
                }
                genus.entry(g).or_default().push(h);
            }
            if let Some(c) = t.crown.filter(|&c| c > 0.0) {
                ratio.push(f64::from(c / h));
            }
        }
        fn medians<K: std::hash::Hash + Eq>(m: HashMap<K, Vec<f32>>) -> HashMap<K, f32> {
            m.into_iter()
                .filter(|(_, v)| v.len() >= MIN_BIN)
                .map(|(k, mut v)| {
                    v.sort_by(f32::total_cmp);
                    (k, v[v.len() / 2])
                })
                .collect()
        }
        ratio.sort_by(f64::total_cmp);
        let crown_per_height = if ratio.is_empty() { 0.5 } else { ratio[ratio.len() / 2] };
        Self {
            crown: medians(crown),
            girth: medians(girth),
            genus: medians(genus),
            crown_per_height,
        }
    }

    fn estimate(&self, genus: &str, crown: Option<f32>, girth: Option<f32>) -> (f32, HeightSource) {
        let gs = [genus.to_string(), String::new()];
        if let Some(c) = crown.filter(|&c| c > 0.0) {
            for g in &gs {
                if let Some(&h) = self.crown.get(&(g.clone(), c.round() as i32)) {
                    return (h, HeightSource::Crown);
                }
            }
        }
        if let Some(u) = girth.filter(|&u| u > 0.0) {
            for g in &gs {
                if let Some(&h) = self.girth.get(&(g.clone(), (u / 25.0).round() as i32)) {
                    return (h, HeightSource::Girth);
                }
            }
        }
        for g in &gs {
            if let Some(&h) = self.genus.get(g) {
                return (h, HeightSource::Genus);
            }
        }
        (10.0, HeightSource::Genus)
    }
}

/// The 3D cadaster, parsed once and cached as `trees/trees3d.json.zst`.
fn trees3d(ctx: &Ctx, out_dir: &Path) -> Result<Vec<Tree3d>> {
    let src = ctx.sources.get("trees3d").context("source trees3d missing")?;
    let files: Vec<PathBuf> =
        src.files.iter().map(|f| ctx.paths.raw().join("trees3d").join(&f.path)).collect();
    let shas: Vec<String> =
        src.files.iter().map(|f| f.sha256.clone().unwrap_or_default()).collect();
    let key = key_of(&[&PREP_VERSION.to_le_bytes(), b"trees3d", shas.join(";").as_bytes()]);
    let cache = out_dir.join("trees3d.json.zst");
    if up_to_date(&cache, &key) {
        return Ok(serde_json::from_slice(&zstd::decode_all(&std::fs::read(&cache)?[..])?)?);
    }
    let mut all = Vec::new();
    for f in &files {
        all.extend(parse_trees3d(f)?);
    }
    info!(trees = all.len(), "3D tree cadaster parsed");
    std::fs::write(&cache, zstd::encode_all(&serde_json::to_vec(&all)?[..], 3)?)?;
    mark(&cache, &key)?;
    Ok(all)
}

fn parse_trees3d(path: &Path) -> Result<Vec<Tree3d>> {
    use quick_xml::events::Event;
    let mut r = quick_xml::Reader::from_reader(BufReader::new(
        std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?,
    ));
    let mut buf = Vec::new();
    let mut out = Vec::new();
    let mut cur: Option<Tree3d> = None;
    let (mut attr, mut in_value, mut in_ref, mut in_pos) = (String::new(), false, false, false);
    loop {
        match r.read_event_into(&mut buf).with_context(|| format!("GML {}", path.display()))? {
            Event::Start(e) => match e.local_name().as_ref() {
                "SolitaryVegetationObject" => {
                    cur = Some(Tree3d {
                        number: String::new(),
                        genus: String::new(),
                        e: f64::NAN,
                        n: f64::NAN,
                        height: None,
                        crown: None,
                        girth: None,
                    })
                }
                "stringAttribute" | "doubleAttribute" | "intAttribute" => {
                    attr = e
                        .attributes()
                        .flatten()
                        .find(|a| a.key.local_name().as_ref() == "name")
                        .map(|a| a.value.to_string())
                        .unwrap_or_default();
                }
                "value" => in_value = true,
                "referencePoint" => in_ref = true,
                "pos" if in_ref => in_pos = true,
                _ => {}
            },
            Event::Text(t) => {
                let Some(c) = cur.as_mut() else { continue };
                let s = t.as_ref().trim();
                if in_value {
                    match attr.as_str() {
                        "Baumnummer" => c.number = s.to_string(),
                        "Gattung" => c.genus = genus_of(s),
                        "Hoehe_aus_ALS" => c.height = s.parse().ok(),
                        "Kronendurchmesser" => c.crown = s.parse().ok(),
                        "Stammumfang" => c.girth = s.parse().ok(),
                        _ => {}
                    }
                } else if in_pos {
                    let mut it = s.split_whitespace().map(str::parse::<f64>);
                    if let (Some(Ok(e)), Some(Ok(n))) = (it.next(), it.next()) {
                        (c.e, c.n) = (e, n);
                    }
                }
            }
            Event::End(e) => match e.local_name().as_ref() {
                "value" => in_value = false,
                "referencePoint" => in_ref = false,
                "pos" => in_pos = false,
                "SolitaryVegetationObject" => {
                    if let Some(c) = cur.take()
                        && !c.number.is_empty()
                    {
                        out.push(c);
                    }
                }
                _ => {}
            },
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }
    Ok(out)
}

/// Street trees of both cadasters with their trunk inside `area`.
fn cadaster(ctx: &Ctx, area: &Bbox) -> Result<Vec<RawTree>> {
    let files = cadaster_files(ctx);
    Ok(hhmc_core::trees::load_cadaster(&files)?
        .into_iter()
        .filter(|t| area.contains(t.e, t.n))
        .map(|t| RawTree {
            id: t.id,
            source: t.source,
            number: t.number,
            e: t.e,
            n: t.n,
            genus: t.genus,
            crown: t.crown,
            girth: t.girth,
        })
        .collect())
}

/// The cadaster GeoJSON files: (source, path).
pub fn cadaster_files(ctx: &Ctx) -> Vec<(TreeSource, PathBuf)> {
    let mut out = Vec::new();
    for (sid, source) in [("trees", TreeSource::Street), ("trees_hpa", TreeSource::Port)] {
        if let Some(src) = ctx.sources.get(sid) {
            out.extend(src.files.iter().map(|f| (source, ctx.paths.raw().join(sid).join(&f.path))));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_is_exact_on_a_line() {
        let p: Vec<(f64, f64)> =
            (0..10).map(|i| (f64::from(i), 2.0 + 0.5 * f64::from(i))).collect();
        let (a, b, r2) = linear_fit(&p);
        assert!((a - 2.0).abs() < 1e-9 && (b - 0.5).abs() < 1e-9 && (r2 - 1.0).abs() < 1e-9);
    }

    #[test]
    fn snap_takes_the_nearest_free_cell() {
        // Free cells only at column 7: the nearest from (5, 5) is (7, 5), 2 m away.
        let s = snap(5, 5, 2.0, |c, _| c == 7).unwrap();
        assert_eq!(s, (7, 5, 2.0));
        assert!(snap(5, 5, 1.5, |c, _| c == 7).is_none());
    }

    #[test]
    fn dilation_is_square() {
        let mut g = Grid::filled(&Bbox::new(0.0, 0.0, 7.0, 7.0), false);
        g.set(3, 3, true);
        let d = dilate(&g, 1);
        assert_eq!(d.data.iter().filter(|&&v| v).count(), 9);
        assert!(d.get(2, 2) && d.get(4, 4) && !d.get(5, 3));
    }
}
