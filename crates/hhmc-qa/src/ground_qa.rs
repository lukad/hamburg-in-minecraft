//! Ground and water checks on written region files (independent of the in-memory build):
//! - ground height: every land column's top ground block vs DGM1;
//! - water: every water-body column holds water from its bed up to the body's level, with
//!   air above, and no bed walls where DGM-W stops (the DGM1/DGM-W seam);
//! - a top-down render per tile (palette colors, water tinted by depth, hillshade).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use hhmc_core::config::{HeightMapping, Origin};
use hhmc_core::landcfg::BodiesFile;
use hhmc_core::raster::{NODATA, read_f32, read_u8, read_u16};
use hhmc_core::voxel::{MAX_Y_EXCL, MIN_Y};
use hhmc_core::{Paths, TILE_SIZE, TileKey, VoxelTile};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};

use crate::palette::Rgb;

#[derive(Clone, Debug, Deserialize)]
pub struct GroundQaConfig {
    pub tolerance_m: f64,
    pub seam_max_step_m: i32,
    /// Median |highest building block − bDOM| over a tile's building columns (target: 1 m).
    pub roof_median_abs_max_m: f64,
    /// Buildings whose own median |top − bDOM| exceeds this are treated as changed since
    /// the reference and reported instead of checked.
    pub roof_changed_m: f64,
    /// Surface height: a column is off when its highest block differs from bDOM by more
    /// than `surface_tolerance_m`; a tile fails when more than `surface_max_off_fraction`
    /// of its columns are.
    pub surface_tolerance_m: f64,
    pub surface_max_off_fraction: f64,
    /// The fraction fails a tile only with at least this many checked columns.
    #[serde(default)]
    pub surface_min_columns: usize,
    /// Surface classes whose columns are checked only where a building stands (yards
    /// whose goods and vehicles bDOM sees but no source models).
    #[serde(default)]
    pub surface_skip_yards: Vec<String>,
}

/// `[ground]` of config/qa.toml.
pub fn load_config(paths: &Paths) -> Result<GroundQaConfig> {
    #[derive(Deserialize)]
    struct QaToml {
        ground: GroundQaConfig,
    }
    Ok(hhmc_core::config::load_toml::<QaToml>(&paths.config().join("qa.toml"))?.ground)
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct BuildingQaResult {
    pub buildings: usize,
    /// Buildings whose geometry produced no voxel column at all.
    pub empty: Vec<String>,
    pub columns: usize,
    pub blocks: usize,
    pub odd_columns: usize,
    /// Buildings with non-watertight geometry (odd ray parity, filled from the ground to
    /// the highest hit): (id, columns).
    pub odd_buildings: Vec<(String, usize)>,
    /// Buildings whose every column (in this tile) floats above the ground, excluding
    /// buildings standing over water (pontoons): (id, median ground y, median underside y).
    pub floating: Vec<(String, i32, i32)>,
    /// Floating buildings over water (pontoons), informational.
    pub floating_on_water: usize,
    /// Buildings excluded from the roof check because their median |top − bDOM| exceeds
    /// `roof_changed_m`: built, demolished or rebuilt since the bDOM (2022) flight.
    /// (id, median |diff|, median signed diff (model − bDOM), columns).
    pub roof_changed: Vec<(String, f64, f64, usize)>,
    /// Buildings whose model underside was more than 1 block below / above the ground
    /// before snapping (informational: snapping fixes both).
    pub raw_sunk: usize,
    pub raw_floating: usize,
    /// Building columns with bDOM data, and |highest building block − bDOM| statistics.
    pub roof_columns: usize,
    pub roof_median_abs_m: f64,
    pub roof_p90_abs_m: f64,
    pub roof_median_signed_m: f64,
    /// Histogram of |highest building block − bDOM| in 0.1 m bins (last bin: ≥ 50 m), for
    /// the area-wide median.
    pub roof_abs_hist: Vec<u32>,
    /// Textured buildings and their wall voxels by material: curtain glass, window glass,
    /// palette blocks.
    pub textured_buildings: usize,
    pub curtain_voxels: usize,
    pub window_voxels: usize,
    pub textured_wall_voxels: usize,
    /// Buildings with the most curtain-wall voxels: (id, curtain, all textured wall voxels).
    pub top_curtain: Vec<(String, usize, usize)>,
    /// Roof color vs DOP20 (CIEDE2000 between the top block's palette color and the
    /// orthophoto pixel), for buildings with textures (LoD3) and without (LoD2).
    pub roof_de_textured_median: f32,
    pub roof_de_untextured_median: f32,
    /// Histogram of roof ΔE2000 in 1-unit bins (last bin ≥ 100), textured buildings.
    pub roof_de_hist: Vec<u32>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct GroundQaResult {
    pub tile: [i32; 2],
    pub land_columns: usize,
    pub land_fail: usize,
    pub land_max_abs_diff_m: f64,
    pub land_mean_abs_diff_m: f64,
    pub water_columns: usize,
    pub water_holes: usize,
    pub water_above_level: usize,
    pub seam_pairs: usize,
    pub seam_walls: usize,
    /// Water columns with a side neighbor whose surface is lower than the water: a
    /// visible, unsupported water wall.
    pub water_walls: usize,
    /// Up to 20 world positions (x, water top y, z) of water walls, for inspection.
    pub water_wall_samples: Vec<[i32; 3]>,
    /// Columns (any, incl. buildings) whose block at the ground height is air: a hole
    /// in the ground, with up to 20 world positions.
    pub ground_holes: usize,
    pub ground_hole_samples: Vec<[i32; 3]>,
    /// Columns outside the area's boundary, and those of them that hold blocks (must be
    /// none).
    #[serde(default)]
    pub outside_columns: usize,
    #[serde(default)]
    pub outside_nonempty: usize,
    pub buildings: BuildingQaResult,
    #[serde(default)]
    pub trees: crate::tree_qa::TreeQaResult,
    #[serde(default)]
    pub infra: crate::infra_qa::InfraQaResult,
    /// Tile-level roof median above the limit (for review; few buildings make it noisy).
    pub roof_review: bool,
    /// Columns compared with bDOM (no water at the top, not within two cells of a tree,
    /// bDOM known) and those whose
    /// highest block is off by more than the surface tolerance.
    #[serde(default)]
    pub surface_columns: usize,
    #[serde(default)]
    pub surface_off: usize,
    #[serde(default)]
    pub surface_off_fraction: f64,
    pub pass: bool,
}

/// Top of a column: (highest non-air y, highest solid non-water y), ignoring blocks in
/// `skip` (trees).
fn column(t: &VoxelTile, water: u16, skip: &HashSet<u16>, x: usize, z: usize) -> (i32, i32) {
    let mut top = MIN_Y - 1;
    for y in (MIN_Y..MAX_Y_EXCL).rev() {
        let b = t.get(x, y, z);
        if b != hhmc_core::AIR && !skip.contains(&b) {
            top = y;
            break;
        }
    }
    let mut ground = top;
    while ground >= MIN_Y && t.get(x, ground, z) == water {
        ground -= 1;
    }
    (top, ground)
}

/// A tile's stage results from the build cache.
pub struct StageLayers {
    pub buildings: Option<hhmc_buildings::BuildingLayer>,
    pub ground: Option<hhmc_core::ground::GroundModel>,
    pub infra: Option<hhmc_infra::InfraLayer>,
    pub landmarks: Option<hhmc_landmarks::LandmarkLayer>,
    /// For a tile cut by the area's boundary: the inside flags of its columns and the
    /// tile composed without the cut.
    pub unclipped: Option<(Vec<bool>, VoxelTile)>,
}

pub struct TileInputs {
    pub region: PathBuf,
    pub dop: PathBuf,
    pub landuse: PathBuf,
    pub bdom: PathBuf,
    /// The tile's building layer (from the build cache).
    pub buildings: Option<hhmc_buildings::BuildingLayer>,
    /// The tile's ground model (from the build cache).
    pub ground: Option<hhmc_core::ground::GroundModel>,
    /// The tile's prepared tree list.
    pub trees: Option<hhmc_core::trees::TileTrees>,
    /// The tile's infra layer (from the build cache).
    pub infra: Option<hhmc_infra::InfraLayer>,
    /// The tile's landmark layer: its columns are left to the landmark.
    pub landmarks: Option<hhmc_landmarks::LandmarkLayer>,
    /// See [`StageLayers::unclipped`].
    pub unclipped: Option<(Vec<bool>, VoxelTile)>,
    /// World columns along railway tracks (vegetation clearance).
    pub track_corridor: std::collections::HashSet<(i32, i32)>,
    pub dgm1: PathBuf,
    pub dgmw: PathBuf,
    pub water: PathBuf,
}

/// Everything a tile check needs besides the tile's own files.
pub struct CheckCtx<'a> {
    pub levels: &'a [Option<f64>],
    pub height: &'a HeightMapping,
    pub cfg: &'a GroundQaConfig,
    pub is_water_class: &'a dyn Fn(u8) -> bool,
    pub colors: &'a HashMap<String, Rgb>,
    pub origin: &'a Origin,
    pub blocks: &'a hhmc_core::landcfg::BlocksConfig,
    pub veg: &'a hhmc_core::trees::VegetationConfig,
    /// The published street tree cadasters.
    pub cadaster: &'a [hhmc_core::trees::CadasterTree],
    pub infra: &'a hhmc_core::infra::InfraConfig,
}

pub fn check_tile(
    key: TileKey,
    inp: &TileInputs,
    cx: &CheckCtx,
    png_out: &Path,
) -> Result<GroundQaResult> {
    let (levels, height, cfg, is_water_class, colors) =
        (cx.levels, cx.height, cx.cfg, cx.is_water_class, cx.colors);
    let landuse = read_u8(&inp.landuse)?;
    let size = TILE_SIZE as usize;
    // Columns covered by a building are excluded from the terrain and water checks.
    let mut bmask = vec![false; size * size];
    if let Some(l) = &inp.buildings {
        for &(x, z, _, _) in &l.clears {
            bmask[z as usize * size + x as usize] = true;
        }
    }
    // Infrastructure columns (decks, piers, rails) too.
    if let Some(il) = &inp.infra {
        for &(x, _, z, _) in &il.blocks {
            bmask[z as usize * size + x as usize] = true;
        }
        for &(x, z, _, _) in &il.clears {
            bmask[z as usize * size + x as usize] = true;
        }
    }
    // Landmarks (footprints and placed blocks) too.
    if let Some(ll) = &inp.landmarks {
        for &(x, _, z, _) in &ll.blocks {
            bmask[z as usize * size + x as usize] = true;
        }
        for &(x, z, _, _) in &ll.clears {
            bmask[z as usize * size + x as usize] = true;
        }
    }
    let mut t = hhmc_world::read_tile(&inp.region, key)
        .with_context(|| format!("reading {}", inp.region.display()))?;
    let mut r = GroundQaResult { tile: [key.rx, key.rz], ..Default::default() };
    // Cut at the area's boundary: columns outside must be empty. The checks then see
    // them as composed without the cut, so that whatever the boundary cuts is judged
    // whole; the render shows them empty.
    let mut outside = vec![false; size * size];
    if let Some((inside, full)) = &inp.unclipped {
        let ids: Vec<u16> = full.palette.states().iter().map(|s| t.intern(s)).collect();
        for (k, _) in inside.iter().enumerate().filter(|(_, v)| !**v) {
            let (x, z) = (k % size, k / size);
            outside[k] = true;
            bmask[k] = true;
            r.outside_columns += 1;
            if (MIN_Y..MAX_Y_EXCL).any(|y| t.get(x, y, z) != hhmc_core::AIR) {
                r.outside_nonempty += 1;
            }
            for y in MIN_Y..MAX_Y_EXCL {
                t.set(x, y, z, ids[full.get(x, y, z) as usize]);
            }
        }
    }
    let dgm1 = read_f32(&inp.dgm1)?;
    let dgmw = read_f32(&inp.dgmw)?;
    let body = read_u16(&inp.water)?;
    let water_id = t
        .palette
        .states()
        .iter()
        .position(|s| &*s.name == "minecraft:water")
        .map_or(u16::MAX, |i| i as u16);
    let b = (dgm1.w - TILE_SIZE as usize) / 2;
    // Terrain and water checks look through trees; the render shows them.
    let trees = crate::tree_qa::tree_block_ids(&t, cx.veg);
    let mut tops = vec![0i32; size * size];
    let mut grounds = vec![0i32; size * size];
    let (mut vis_tops, mut vis_grounds) = (vec![0i32; size * size], vec![0i32; size * size]);
    let mut sum_diff = 0.0;
    // Heatmap: land columns by |ground − DGM1|, water columns ok or holed, black where
    // not checked (buildings, infrastructure).
    let mut gimg = vec![0u8; size * size * 3];
    let mut paint = |x: usize, z: usize, c: [u8; 3]| {
        gimg[(z * size + x) * 3..(z * size + x) * 3 + 3].copy_from_slice(&c);
    };
    for z in 0..size {
        for x in 0..size {
            let i = (z + b) * dgm1.w + x + b;
            let (top, ground) = column(&t, water_id, &trees, x, z);
            tops[z * size + x] = top;
            grounds[z * size + x] = ground;
            (vis_tops[z * size + x], vis_grounds[z * size + x]) = if outside[z * size + x] {
                (MIN_Y - 1, MIN_Y - 1)
            } else {
                column(&t, water_id, &HashSet::new(), x, z)
            };
            if bmask[z * size + x] {
                continue;
            }
            match levels.get(body.data[i] as usize).copied().flatten() {
                None => {
                    if dgm1.data[i] == NODATA {
                        continue;
                    }
                    let m = height.to_y(f64::from(dgm1.data[i]));
                    if m.clamped {
                        continue;
                    }
                    r.land_columns += 1;
                    let d = (f64::from(ground) - f64::from(dgm1.data[i])).abs();
                    sum_diff += d;
                    r.land_max_abs_diff_m = r.land_max_abs_diff_m.max(d);
                    if d > cfg.tolerance_m + 1e-6 || top != ground {
                        r.land_fail += 1;
                        paint(x, z, [230, 30, 30]);
                    } else {
                        let g = (60.0 + 120.0 * (1.0 - d / cfg.tolerance_m.max(1e-6))) as u8;
                        paint(x, z, [40, g, 40]);
                    }
                }
                Some(level) => {
                    r.water_columns += 1;
                    let ly = height.to_y(level).y;
                    if ground < ly {
                        // Water from ground + 1 up to the level, air above.
                        let full = (ground + 1..=ly).all(|y| t.get(x, y, z) == water_id);
                        if !full || top != ly {
                            r.water_holes += 1;
                            paint(x, z, [255, 0, 255]);
                        } else {
                            paint(x, z, [40, 70, 150]);
                        }
                    } else if top != ground {
                        r.water_above_level += 1;
                    }
                }
            }
        }
    }
    r.land_mean_abs_diff_m =
        if r.land_columns > 0 { sum_diff / r.land_columns as f64 } else { 0.0 };
    let dir = png_out.parent().expect("tile dir");
    write_png(&dir.join("ground.png"), size, size, &gimg)?;
    // Surface height: the highest block of every column against bDOM. Water surfaces
    // aside (bDOM is unreliable over water), and tree crowns: bDOM comes from the same
    // leaf-off flight as DOP20 and partly sees through bare crowns, while real crowns are
    // wider than the modeled ones (trees have their own checks).
    let bdom = read_f32(&inp.bdom)?;
    // Crown fringes: within two cells of a tree column (real crowns are wider than the
    // modeled ones).
    let mut near_tree = vec![false; size * size];
    for z in 0..size {
        for x in 0..size {
            let top = vis_tops[z * size + x];
            if top < MIN_Y || !trees.contains(&t.get(x, top, z)) {
                continue;
            }
            for nz in z.saturating_sub(2)..(z + 3).min(size) {
                for nx in x.saturating_sub(2)..(x + 3).min(size) {
                    near_tree[nz * size + nx] = true;
                }
            }
        }
    }
    let mut simg = vec![0u8; size * size * 3];
    let yard_classes: Vec<u8> =
        cfg.surface_skip_yards.iter().filter_map(|n| cx.blocks.class_index(n).ok()).collect();
    let mut building_col = vec![false; size * size];
    if let Some(l) = &inp.buildings {
        for &(x, z, _, _) in &l.clears {
            building_col[z as usize * size + x as usize] = true;
        }
    }
    for z in 0..size {
        for x in 0..size {
            let k = z * size + x;
            let v = bdom.data[(z + b) * bdom.w + x + b];
            let top = vis_tops[k];
            if v == NODATA || top < MIN_Y || t.get(x, top, z) == water_id || near_tree[k] {
                continue;
            }
            let class = landuse.data[(z + b) * landuse.w + x + b];
            if yard_classes.contains(&class) && !building_col[k] {
                continue;
            }
            r.surface_columns += 1;
            let d = f64::from(top) - f64::from(v);
            if d.abs() > cfg.surface_tolerance_m {
                r.surface_off += 1;
            }
            // Model above bDOM red, below blue; saturated at 6 m.
            let a = (d.abs() / 6.0).min(1.0);
            let c = if d.abs() <= cfg.surface_tolerance_m {
                [90, 90, 90]
            } else if d > 0.0 {
                [(120.0 + 135.0 * a) as u8, 40, 40]
            } else {
                [40, 40, (120.0 + 135.0 * a) as u8]
            };
            simg[k * 3..k * 3 + 3].copy_from_slice(&c);
        }
    }
    r.surface_off_fraction =
        if r.surface_columns > 0 { r.surface_off as f64 / r.surface_columns as f64 } else { 0.0 };
    write_png(&dir.join("surface.png"), size, size, &simg)?;
    // The orthophoto beside the render.
    if inp.dop.exists() {
        let dop = hhmc_core::raster::read_rgba(&inp.dop)?;
        let bd = (dop.w - size) / 2;
        let mut img = vec![0u8; size * size * 3];
        for z in 0..size {
            for x in 0..size {
                let px = dop.data[(z + bd) * dop.w + x + bd];
                img[(z * size + x) * 3..(z * size + x) * 3 + 3].copy_from_slice(&px[..3]);
            }
        }
        write_png(&dir.join("dop.png"), size, size, &img)?;
    }
    // Seam: neighboring ALKIS water columns, one with a DGM-W bed and one with a
    // synthetic bed (flooded land keeps its DGM1 bed and is not part of the seam).
    for z in 0..size {
        for x in 0..size {
            let i = (z + b) * dgm1.w + x + b;
            if body.data[i] == 0 || !is_water_class(landuse.data[i]) || bmask[z * size + x] {
                continue;
            }
            for (dx, dz) in [(1usize, 0usize), (0, 1)] {
                let (nx, nz) = (x + dx, z + dz);
                if nx >= size || nz >= size {
                    continue;
                }
                let j = (nz + b) * dgm1.w + nx + b;
                if body.data[j] == 0
                    || bmask[nz * size + nx]
                    || !is_water_class(landuse.data[j])
                    || (dgmw.data[i] == NODATA) == (dgmw.data[j] == NODATA)
                {
                    continue;
                }
                r.seam_pairs += 1;
                if (grounds[z * size + x] - grounds[nz * size + nx]).abs() > cfg.seam_max_step_m {
                    r.seam_walls += 1;
                }
            }
        }
    }
    // Water walls: a water surface higher than a neighboring column's surface.
    for z in 0..size {
        for x in 0..size {
            let k = z * size + x;
            if tops[k] == grounds[k] || bmask[k] {
                continue;
            }
            // Columns outside the area's boundary are not in the world (their composition
            // is only filled in for the checks), so the cut face is not a wall.
            let lower = [(1i64, 0i64), (-1, 0), (0, 1), (0, -1)].iter().any(|(dx, dz)| {
                let (nx, nz) = (x as i64 + dx, z as i64 + dz);
                (0..size as i64).contains(&nx)
                    && (0..size as i64).contains(&nz)
                    && !outside[nz as usize * size + nx as usize]
                    && tops[nz as usize * size + nx as usize] < tops[k]
            });
            if lower {
                r.water_walls += 1;
                if r.water_wall_samples.len() < 20 {
                    let (bx, bz) = key.min_block();
                    r.water_wall_samples.push([bx + x as i32, tops[k], bz + z as i32]);
                }
            }
        }
    }
    // Holes: the ground block (from the ground model's height = DGM1 on land) is air.
    // Track beds cut into the ground lower it on purpose.
    let cut: std::collections::HashSet<(u16, u16)> =
        inp.infra.iter().flat_map(|il| il.clears.iter().map(|c| (c.0, c.1))).collect();
    if let Some(gm) = &inp.ground {
        for z in 0..size {
            for x in 0..size {
                if cut.contains(&(x as u16, z as u16)) || outside[z * size + x] {
                    continue;
                }
                let gy = i32::from(gm.ground[gm.idx(x, z)]);
                let b = t.get(x, gy, z);
                // Air, or see-through glass, where the ground should be.
                let see_through = b == hhmc_core::AIR || t.palette.get(b).name.contains("glass");
                if gm.water_top[gm.idx(x, z)] == hhmc_core::ground::NO_WATER && see_through {
                    r.ground_holes += 1;
                    if r.ground_hole_samples.len() < 20 {
                        let (bx, bz) = key.min_block();
                        r.ground_hole_samples.push([bx + x as i32, gy, bz + z as i32]);
                    }
                }
            }
        }
    }
    if let Some(l) = &inp.buildings {
        r.buildings = building_checks(l, cfg.roof_changed_m, &bmask, &tops, &bdom, b);
        if inp.dop.exists() {
            let dop = hhmc_core::raster::read_rgba(&inp.dop)?;
            roof_color(&mut r.buildings, l, &t, &tops, &dop, b, colors);
        }
    }
    if let (Some(gm), Some(tt)) = (&inp.ground, &inp.trees) {
        let tops = match &inp.buildings {
            Some(l) => hhmc_core::trees::building_tops(&l.clears, &l.blocks),
            None => vec![i16::MIN; size * size],
        };
        r.trees = crate::tree_qa::check_trees(&crate::tree_qa::TreeCheckInputs {
            key,
            origin: cx.origin,
            tile: &t,
            ground: gm,
            building_top: &tops,
            trees: tt,
            cadaster: cx.cadaster,
            veg: cx.veg,
            blocks: cx.blocks,
            track_corridor: &inp.track_corridor,
            stands_on: &inp.infra.as_ref().map(|l| l.abutment_tops()).unwrap_or_default(),
        });
    } else {
        r.trees.pass = true;
    }
    r.infra.pass = true;
    let surface_classes: Vec<u8> = cx
        .infra
        .surfaces
        .classes
        .iter()
        .filter_map(|n| cx.blocks.classes.iter().position(|c| &c.id == n).map(|i| i as u8))
        .collect();
    let surface_tops: Vec<String> = surface_classes
        .iter()
        .map(|&c| {
            let t = &cx.blocks.classes[c as usize].top;
            t.split('[').next().unwrap_or(t).to_string()
        })
        .collect();
    if let (Some(gm), Some(il)) = (&inp.ground, &inp.infra) {
        let tops = match &inp.buildings {
            Some(l) => hhmc_core::trees::building_tops(&l.clears, &l.blocks),
            None => vec![i16::MIN; size * size],
        };
        r.infra = crate::infra_qa::check_infra(&crate::infra_qa::InfraCheckInputs {
            key,
            tile: &t,
            ground: gm,
            infra: il,
            cfg: cx.infra,
            building_top: &tops,
            anchor_tolerance: cx.infra.heights.anchor_tolerance_m.floor() as i32,
            surface_classes: &surface_classes,
            surface_tops: &surface_tops,
            ballast: cx.infra.rails.ballast.clone(),
        });
    }
    r.pass = r.trees.pass
        && r.infra.pass
        && r.ground_holes == 0
        && r.outside_nonempty == 0
        && r.land_fail == 0
        && r.water_holes == 0
        && r.seam_walls == 0
        && r.water_walls == 0
        && r.buildings.floating.is_empty()
        && (r.surface_off_fraction <= cfg.surface_max_off_fraction
            || r.surface_columns < cfg.surface_min_columns);
    // A tile median above the limit is a review flag; the pass criterion is area-wide.
    r.roof_review =
        r.buildings.roof_columns > 0 && r.buildings.roof_median_abs_m > cfg.roof_median_abs_max_m;
    render_top(&t, &vis_tops, &vis_grounds, water_id, colors, png_out)?;
    Ok(r)
}

fn building_checks(
    l: &hhmc_buildings::BuildingLayer,
    changed_m: f64,
    bmask: &[bool],
    tops: &[i32],
    bdom: &hhmc_core::raster::Grid<f32>,
    border: usize,
) -> BuildingQaResult {
    let size = TILE_SIZE as usize;
    let mut r = BuildingQaResult {
        buildings: l.stats.len(),
        empty: l.empty.clone(),
        columns: l.stats.iter().map(|s| s.columns_in_tile).sum(),
        blocks: l.stats.iter().map(|s| s.blocks_in_tile).sum(),
        odd_columns: l.stats.iter().map(|s| s.odd_columns).sum(),
        ..Default::default()
    };
    for s in &l.stats {
        r.textured_buildings += usize::from(s.textured);
        r.curtain_voxels += s.curtain_voxels;
        r.window_voxels += s.window_voxels;
        r.textured_wall_voxels += s.textured_wall_voxels;
        if s.odd_columns > 0 {
            r.odd_buildings.push((s.id.clone(), s.odd_columns));
        }
        if s.columns_in_tile >= 4 && s.floating_columns == s.columns_in_tile {
            if s.water_columns * 2 >= s.columns_in_tile {
                r.floating_on_water += 1;
            } else {
                r.floating.push((s.id.clone(), s.median_ground, s.median_bottom));
            }
        }
        r.raw_sunk += usize::from(s.raw_base_offset < -1);
        r.raw_floating += usize::from(s.raw_base_offset > 1);
    }
    let _ = bmask;
    let mut tc: Vec<_> = l
        .stats
        .iter()
        .filter(|s| s.curtain_voxels > 0)
        .map(|s| {
            (
                s.id.clone(),
                s.curtain_voxels,
                s.curtain_voxels + s.window_voxels + s.textured_wall_voxels,
            )
        })
        .collect();
    tc.sort_by_key(|t| std::cmp::Reverse(t.1));
    tc.truncate(5);
    r.top_curtain = tc;
    let mut diffs: Vec<f64> = Vec::new();
    for s in &l.stats {
        let mut d: Vec<f64> = l.clears[s.clears.0 as usize..s.clears.1 as usize]
            .iter()
            .filter_map(|&(x, z, _, _)| {
                let v = bdom.data[(z as usize + border) * bdom.w + x as usize + border];
                (v != NODATA)
                    .then(|| f64::from(tops[z as usize * size + x as usize]) - f64::from(v))
            })
            .collect();
        if d.is_empty() {
            continue;
        }
        let mut abs: Vec<f64> = d.iter().map(|v| v.abs()).collect();
        abs.sort_by(f64::total_cmp);
        let med = abs[abs.len() / 2];
        if med > changed_m {
            d.sort_by(f64::total_cmp);
            let signed = d[d.len() / 2];
            r.roof_changed.push((
                s.id.clone(),
                (med * 10.0).round() / 10.0,
                (signed * 10.0).round() / 10.0,
                d.len(),
            ));
            continue;
        }
        diffs.append(&mut d);
    }
    r.roof_columns = diffs.len();
    r.roof_abs_hist = vec![0; 501];
    for d in &diffs {
        r.roof_abs_hist[((d.abs() * 10.0) as usize).min(500)] += 1;
    }
    if !diffs.is_empty() {
        diffs.sort_by(f64::total_cmp);
        r.roof_median_signed_m = diffs[diffs.len() / 2];
        let mut abs: Vec<f64> = diffs.iter().map(|d| d.abs()).collect();
        abs.sort_by(f64::total_cmp);
        r.roof_median_abs_m = abs[abs.len() / 2];
        r.roof_p90_abs_m = abs[abs.len() * 9 / 10];
    }
    r
}

/// ΔE2000 between each building column's top block color and the orthophoto.
fn roof_color(
    r: &mut BuildingQaResult,
    l: &hhmc_buildings::BuildingLayer,
    t: &VoxelTile,
    tops: &[i32],
    dop: &hhmc_core::raster::Grid<[u8; 4]>,
    border: usize,
    colors: &HashMap<String, Rgb>,
) {
    use palette::color_difference::Ciede2000;
    let size = TILE_SIZE as usize;
    let lab = |c: [u8; 3]| -> palette::Lab {
        palette::IntoColor::into_color(
            palette::Srgb::new(c[0], c[1], c[2]).into_format::<f32>().into_linear::<f32>(),
        )
    };
    let (mut tex, mut untex) = (Vec::new(), Vec::new());
    r.roof_de_hist = vec![0; 101];
    for s in &l.stats {
        for &(x, z, _, _) in &l.clears[s.clears.0 as usize..s.clears.1 as usize] {
            let (x, z) = (x as usize, z as usize);
            let px = dop.data[(z + border) * dop.w + x + border];
            if px[3] < 128 {
                continue;
            }
            let y = tops[z * size + x];
            let Some(c) = colors.get(&t.palette.get(t.get(x, y, z)).name) else { continue };
            let de = lab(*c).difference(lab([px[0], px[1], px[2]]));
            if s.textured {
                tex.push(de);
                r.roof_de_hist[(de as usize).min(100)] += 1;
            } else {
                untex.push(de);
            }
        }
    }
    let med = |v: &mut Vec<f32>| {
        v.sort_by(f32::total_cmp);
        v.get(v.len() / 2).copied().unwrap_or(0.0)
    };
    r.roof_de_textured_median = med(&mut tex);
    r.roof_de_untextured_median = med(&mut untex);
}

fn render_top(
    t: &VoxelTile,
    tops: &[i32],
    grounds: &[i32],
    water_id: u16,
    colors: &HashMap<String, Rgb>,
    out: &Path,
) -> Result<()> {
    let size = TILE_SIZE as usize;
    write_png(out, size, size, &top_rgb(t, tops, grounds, water_id, colors))
}

/// A tile's top-down view (palette colors, water tinted by depth, hillshade) and the
/// highest non-air block of every column (`MIN_Y - 1` where there is none).
pub fn top_view(t: &VoxelTile, colors: &HashMap<String, Rgb>) -> (Vec<u8>, Vec<i32>) {
    let size = TILE_SIZE as usize;
    let water_id = t
        .palette
        .states()
        .iter()
        .position(|s| &*s.name == "minecraft:water")
        .map_or(u16::MAX, |i| i as u16);
    let (mut tops, mut grounds) = (vec![0i32; size * size], vec![0i32; size * size]);
    for z in 0..size {
        for x in 0..size {
            (tops[z * size + x], grounds[z * size + x]) =
                column(t, water_id, &HashSet::new(), x, z);
        }
    }
    (top_rgb(t, &tops, &grounds, water_id, colors), tops)
}

fn top_rgb(
    t: &VoxelTile,
    tops: &[i32],
    grounds: &[i32],
    water_id: u16,
    colors: &HashMap<String, Rgb>,
) -> Vec<u8> {
    let size = TILE_SIZE as usize;
    let water_c = colors.get("minecraft:water").copied().unwrap_or([43, 81, 158]);
    let mut img = vec![0u8; size * size * 3];
    let color_of = |x: usize, y: i32, z: usize| -> Rgb {
        let s = t.palette.get(t.get(x, y, z));
        colors.get(&s.name).copied().unwrap_or([255, 0, 255])
    };
    for z in 0..size {
        for x in 0..size {
            let k = z * size + x;
            let (top, ground) = (tops[k], grounds[k]);
            let mut c = if top < MIN_Y { [0, 0, 0] } else { color_of(x, ground.max(MIN_Y), z) };
            if top > ground && t.get(x, top, z) == water_id {
                let depth = f64::from(top - ground);
                let a = (0.55 + depth * 0.05).min(0.9);
                c = [0, 1, 2]
                    .map(|i| (f64::from(c[i]) * (1.0 - a) + f64::from(water_c[i]) * a) as u8);
            }
            // Hillshade from the surface (light from the north-west).
            let h = |dx: i64, dz: i64| {
                let (nx, nz) = (
                    (x as i64 + dx).clamp(0, size as i64 - 1),
                    (z as i64 + dz).clamp(0, size as i64 - 1),
                );
                f64::from(tops[nz as usize * size + nx as usize])
            };
            let slope = (h(-1, 0) - h(1, 0)) + (h(0, -1) - h(0, 1));
            let shade = (1.0 + 0.06 * slope).clamp(0.6, 1.4);
            for i in 0..3 {
                img[k * 3 + i] = (f64::from(c[i]) * shade).clamp(0.0, 255.0) as u8;
            }
        }
    }
    img
}

pub fn write_png(path: &Path, w: usize, h: usize, rgb: &[u8]) -> Result<()> {
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    let f = std::io::BufWriter::new(std::fs::File::create(path)?);
    let mut enc = png::Encoder::new(f, w as u32, h as u32);
    enc.set_color(png::ColorType::Rgb);
    enc.set_depth(png::BitDepth::Eight);
    let mut wr = enc.write_header()?;
    wr.write_image_data(rgb)?;
    Ok(())
}

/// Runs the ground checks on all `tiles`, writes per-tile JSON and PNG under `out/qa/` and
/// an area overview (see [`overview`]); returns the results.
pub fn run(
    paths: &Paths,
    tiles: &[TileKey],
    origin: &Origin,
    height: &HeightMapping,
    region_dir: &Path,
    layers: &(dyn Fn(TileKey) -> Result<StageLayers> + Sync),
) -> Result<Vec<GroundQaResult>> {
    #[derive(Deserialize)]
    struct QaToml {
        ground: GroundQaConfig,
    }
    let cfg: QaToml = hhmc_core::config::load_toml(&paths.config().join("qa.toml"))?;
    let colors = crate::palette::load(&paths.config().join("block_colors.toml"))?;
    let levels = BodiesFile::load(paths)?.levels();
    let blocks = hhmc_core::landcfg::BlocksConfig::load(paths)?;
    let is_water_class = |c: u8| blocks.is_water(c);
    let veg = hhmc_core::trees::VegetationConfig::load(paths)?;
    let infra_cfg = hhmc_core::infra::InfraConfig::load(paths)?;
    let sources = cadaster_files(paths)?;
    let cadaster = hhmc_core::trees::load_cadaster(&sources)?;
    let qa = paths.out().join("qa");
    let prep =
        |id: &str, t: TileKey| paths.prepared().join(id).join(format!("{}_{}.tif", t.rx, t.rz));
    let results: Vec<GroundQaResult> = tiles
        .par_iter()
        .map(|&t| {
            let StageLayers { buildings: bl, ground: gm, infra: il, landmarks: ll, unclipped } =
                layers(t)?;
            let mut inp = TileInputs {
                region: region_dir.join(t.region_file_name()),
                dop: prep("dop20", t),
                landuse: prep("landuse", t),
                bdom: prep("bdom1", t),
                buildings: None,
                ground: None,
                dgm1: prep("dgm1", t),
                dgmw: prep("dgmw", t),
                water: prep("water", t),
                trees: None,
                infra: il,
                landmarks: ll,
                unclipped,
                track_corridor: Default::default(),
            };
            let structures = hhmc_core::infra::TileInfra::path(paths, t);
            if structures.exists() {
                inp.track_corridor = hhmc_core::infra::track_corridor(
                    &hhmc_core::infra::TileInfra::load(&structures)?.tracks,
                    origin,
                    infra_cfg.rails.corridor_m,
                );
            }
            let list = hhmc_core::trees::TileTrees::path(paths, t);
            inp.trees =
                if list.exists() { Some(hhmc_core::trees::TileTrees::load(&list)?) } else { None };
            inp.buildings = bl;
            inp.ground = gm;
            let dir = qa.join("tiles").join(format!("{}_{}", t.rx, t.rz));
            let cx = CheckCtx {
                levels: &levels,
                height,
                cfg: &cfg.ground,
                is_water_class: &is_water_class,
                colors: &colors,
                origin,
                blocks: &blocks,
                veg: &veg,
                cadaster: &cadaster,
                infra: &infra_cfg,
            };
            let r = check_tile(t, &inp, &cx, &dir.join("top.png"))?;
            std::fs::write(dir.join("ground.json"), serde_json::to_string_pretty(&r)? + "\n")?;
            Ok(r)
        })
        .collect::<Result<_>>()?;
    overview(&qa, tiles, origin)?;
    std::fs::write(qa.join("ground.json"), serde_json::to_string_pretty(&results)? + "\n")?;
    Ok(results)
}

/// The street tree cadasters' files from data/sources.toml.
fn cadaster_files(paths: &Paths) -> Result<Vec<(hhmc_core::trees::TreeSource, PathBuf)>> {
    use hhmc_core::trees::TreeSource;
    let sources = hhmc_fetch::Sources::load(&paths.sources_toml())?;
    let mut out = Vec::new();
    for (sid, source) in [("trees", TreeSource::Street), ("trees_hpa", TreeSource::Port)] {
        if let Some(src) = sources.get(sid) {
            out.extend(src.files.iter().map(|f| (source, paths.raw().join(sid).join(&f.path))));
        }
    }
    Ok(out)
}

/// Largest side of the overview image in pixels.
const OVERVIEW_MAX_PX: usize = 8192;

/// Stitches the per-tile renders into one image, at half resolution or coarser so that
/// it stays within [`OVERVIEW_MAX_PX`].
fn overview(qa: &Path, tiles: &[TileKey], _origin: &Origin) -> Result<()> {
    let (rx0, rx1) = (
        tiles.iter().map(|t| t.rx).min().unwrap_or(0),
        tiles.iter().map(|t| t.rx).max().unwrap_or(0),
    );
    let (rz0, rz1) = (
        tiles.iter().map(|t| t.rz).min().unwrap_or(0),
        tiles.iter().map(|t| t.rz).max().unwrap_or(0),
    );
    let size = TILE_SIZE as usize;
    let span = ((rx1 - rx0 + 1).max(rz1 - rz0 + 1)) as usize * size;
    // Pixels per side of a tile: 256, 128, … (a power of two dividing the tile).
    let mut f = 2;
    while span / f > OVERVIEW_MAX_PX && f < size {
        f *= 2;
    }
    let px = size / f;
    let (w, h) = (((rx1 - rx0 + 1) as usize) * px, ((rz1 - rz0 + 1) as usize) * px);
    let mut img = vec![0u8; w * h * 3];
    for t in tiles {
        let p = qa.join("tiles").join(format!("{}_{}", t.rx, t.rz)).join("top.png");
        let dec = png::Decoder::new(std::io::BufReader::new(std::fs::File::open(&p)?));
        let mut rd = dec.read_info()?;
        let mut buf = vec![0; rd.output_buffer_size().context("png size")?];
        rd.next_frame(&mut buf)?;
        let (ox, oz) = (((t.rx - rx0) as usize) * px, ((t.rz - rz0) as usize) * px);
        for z in 0..px {
            for x in 0..px {
                for c in 0..3 {
                    let mut s = 0u32;
                    for dz in 0..f {
                        for dx in 0..f {
                            s += u32::from(buf[((z * f + dz) * size + x * f + dx) * 3 + c]);
                        }
                    }
                    img[((oz + z) * w + ox + x) * 3 + c] = (s / (f * f) as u32) as u8;
                }
            }
        }
    }
    write_png(&qa.join("overview.png"), w, h, &img)
}
