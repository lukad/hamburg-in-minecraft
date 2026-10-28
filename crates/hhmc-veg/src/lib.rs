//! Vegetation stage: turns the prepared tree list of a tile
//! (`data/prepared/trees/<rx>_<rz>.json`) into blocks.
//!
//! A tree is a trunk of logs (2 × 2 from `big_trunk_girth_cm`) and a crown of persistent
//! leaves: an ellipsoid (or a cone for conifers) spanning the top `crown_share` of the
//! height, as wide as the crown diameter, with its outer shell thinned by deterministic
//! noise seeded by the tree id and world position. Street and port trees stand in a tree
//! pit. Every voxel is placed only if it lies in this tile, above the ground, not in water,
//! not in a building column below the building's top, and (leaves only) above the
//! clearance over roads and railway tracks; so a tree crossing a tile edge comes out identical on both sides.

use std::collections::BTreeMap;

use anyhow::{Context, Result};
use hhmc_core::config::Origin;
use hhmc_core::ground::{GroundModel, NO_WATER};
use hhmc_core::landcfg::BlocksConfig;
use hhmc_core::trees::{CrownShape, TileTrees, Tree, TreeSource, VegetationConfig};
use hhmc_core::{BlockState, TILE_SIZE};
use serde::{Deserialize, Serialize};

/// Bump when the vegetation logic changes.
pub const VEG_VERSION: u32 = 6;

/// Blocks of the vegetation layer, in tile-local coordinates.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct VegLayer {
    pub palette: Vec<BlockState>,
    /// (x, y, z, palette index).
    pub blocks: Vec<(u16, i16, u16, u16)>,
    pub stats: VegStats,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct VegStats {
    /// Trees whose trunk lies in the tile, and neighbors' trees reaching into it.
    pub own: usize,
    pub near: usize,
    pub logs: usize,
    pub leaves: usize,
    pub pit_blocks: usize,
    /// Voxels dropped: inside a building, in water, below the road clearance.
    pub clipped_building: usize,
    pub clipped_water: usize,
    pub clipped_clearance: usize,
    /// Park trees not placed because their trunk is on a railway track.
    pub on_tracks: usize,
}

pub struct VegInputs<'a> {
    pub trees: &'a TileTrees,
    pub cfg: &'a VegetationConfig,
    pub blocks: &'a BlocksConfig,
    pub origin: &'a Origin,
    pub ground: &'a GroundModel,
    /// Highest building block per tile column (`z * 512 + x`), `i16::MIN` if none.
    pub building_top: &'a [i16],
    /// World columns along railway tracks: leaves keep the clearance there too.
    pub track_corridor: &'a std::collections::HashSet<(i32, i32)>,
    /// Deck tops by world column where the street is on a deck above the ground model
    /// (bridge abutments): trees there stand on the deck.
    pub stands_on: &'a std::collections::HashMap<(i32, i32), i32>,
}

pub use hhmc_core::trees::building_tops;

/// Voxel kinds in placement priority order (later wins).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Kind {
    Leaves,
    Log,
}

pub fn vegetation(inp: &VegInputs) -> Result<VegLayer> {
    let cfg = inp.cfg;
    let g = inp.ground;
    let size = TILE_SIZE;
    let (bx, bz) = inp.ground.key.min_block();
    let class_ids = |names: &[String]| -> Result<Vec<u8>> {
        names
            .iter()
            .map(|n| {
                inp.blocks
                    .classes
                    .iter()
                    .position(|c| &c.id == n)
                    .map(|i| i as u8)
                    .with_context(|| format!("vegetation.toml: unknown surface class {n:?}"))
            })
            .collect()
    };
    let clearance = class_ids(&cfg.conflicts.clearance_classes)?;
    let pit_classes = class_ids(&cfg.pit.classes)?;

    let mut palette: Vec<BlockState> = Vec::new();
    let mut intern = |s: BlockState| -> u16 {
        if let Some(i) = palette.iter().position(|p| *p == s) {
            return i as u16;
        }
        palette.push(s);
        (palette.len() - 1) as u16
    };
    let mut fam_ids: BTreeMap<&str, (u16, u16, CrownShape)> = BTreeMap::new();
    for (id, f) in &cfg.family {
        let log = BlockState::parse(&f.log).map_err(anyhow::Error::msg)?.with("axis", "y");
        let leaves = BlockState::parse(&f.leaves)
            .map_err(anyhow::Error::msg)?
            .with("distance", "7")
            .with("persistent", "true")
            .with("waterlogged", "false");
        fam_ids.insert(id.as_str(), (intern(log), intern(leaves), f.shape));
    }
    let pit = intern(BlockState::parse(&cfg.pit.block).map_err(anyhow::Error::msg)?);

    let mut stats =
        VegStats { own: inp.trees.own.len(), near: inp.trees.near.len(), ..Default::default() };
    // (x, y, z) → (kind, block); BTreeMap for a deterministic output order.
    let mut vox: BTreeMap<(u16, i16, u16), (Kind, u16)> = BTreeMap::new();
    let mut pits: BTreeMap<(u16, u16), i16> = BTreeMap::new();
    let mut trees: Vec<&Tree> = inp.trees.own.iter().chain(&inp.trees.near).collect();
    trees.sort_by(|a, b| a.id.cmp(&b.id));
    for t in trees {
        let (wx, wz) = inp.origin.block_at(t.e, t.n);
        // Park trees (from the canopy model) are not placed on railway tracks.
        if t.source == TreeSource::Canopy && inp.track_corridor.contains(&(wx, wz)) {
            stats.on_tracks += 1;
            continue;
        }
        let (lx, lz) = (wx - bx, wz - bz);
        let Some(gi) = g.idx_buffered(lx, lz) else { continue };
        let base =
            i32::from(g.ground[gi]).max(inp.stands_on.get(&(wx, wz)).copied().unwrap_or(i32::MIN));
        let (log, leaves, shape) = fam_ids[cfg.family_of(&t.genus)];
        let seed = hash_str(&t.id);
        // Trunk position in block space (x east, z south), continuous.
        let px = t.e - inp.origin.e0 as f64;
        let pz = inp.origin.n0 as f64 - t.n;

        let h = (t.height_m.round() as i32).max(2);
        let crown_base = ((h as f32 * (1.0 - cfg.tree.crown_share)).round() as i32)
            .max(cfg.tree.min_crown_base_blocks)
            .min(h - 2)
            .max(1);
        let trunk_top = crown_base + ((h - crown_base) as f32 * 0.6).round() as i32;
        let big = t.girth_cm.is_some_and(|c| c >= cfg.tree.big_trunk_girth_cm);
        let trunk_cells: Vec<(i32, i32)> = if big {
            let (x0, z0) = ((px - 1.0).round() as i32, (pz - 1.0).round() as i32);
            vec![(x0, z0), (x0 + 1, z0), (x0, z0 + 1), (x0 + 1, z0 + 1)]
        } else {
            vec![(wx, wz)]
        };

        // Crown.
        let r = f64::from(t.crown_m.max(cfg.tree.min_crown_m)) / 2.0;
        let (y0, y1) = (base + crown_base, base + h);
        let (cy, ay) = (f64::from(y0 + y1) / 2.0, f64::from(y1 - y0) / 2.0 + 0.5);
        let k = r.ceil() as i32 + 1;
        for y in y0..=y1 {
            for dz in -k..=k {
                for dx in -k..=k {
                    let (x, z) = (wx + dx, wz + dz);
                    let (ex, ez) = (f64::from(x) + 0.5 - px, f64::from(z) + 0.5 - pz);
                    let hd = ex.hypot(ez);
                    // Normalized distance from the crown's surface (1 = on it).
                    let rho = match shape {
                        CrownShape::Ellipsoid => {
                            ((hd / r).powi(2) + ((f64::from(y) - cy) / ay).powi(2)).sqrt()
                        }
                        CrownShape::Cone => {
                            let rr = r * f64::from(y1 + 1 - y) / f64::from(y1 + 1 - y0);
                            hd / rr.max(0.5)
                        }
                    };
                    if rho > 1.0 {
                        continue;
                    }
                    // Thin the shell, but keep the column above the trunk so the tree
                    // reaches its full height.
                    if rho > 0.75 && hd > 0.75 && noise(seed, x, y, z) > cfg.tree.edge_density {
                        continue;
                    }
                    put(
                        &mut vox,
                        &mut stats,
                        inp,
                        &clearance,
                        (bx, bz),
                        (x, y, z),
                        Kind::Leaves,
                        leaves,
                    );
                }
            }
        }
        // Trunk.
        for &(x, z) in &trunk_cells {
            for y in base + 1..=base + trunk_top.min(h - 1) {
                put(&mut vox, &mut stats, inp, &clearance, (bx, bz), (x, y, z), Kind::Log, log);
            }
        }
        // Pit.
        if t.source != TreeSource::Canopy {
            let pr = cfg.pit.radius_blocks;
            let (minx, minz) =
                trunk_cells.iter().fold((i32::MAX, i32::MAX), |a, c| (a.0.min(c.0), a.1.min(c.1)));
            let (maxx, maxz) =
                trunk_cells.iter().fold((i32::MIN, i32::MIN), |a, c| (a.0.max(c.0), a.1.max(c.1)));
            for z in minz - pr..=maxz + pr {
                for x in minx - pr..=maxx + pr {
                    let (lx, lz) = (x - bx, z - bz);
                    if !(0..size).contains(&lx) || !(0..size).contains(&lz) {
                        continue;
                    }
                    let i = g.idx(lx as usize, lz as usize);
                    let k = lz as usize * size as usize + lx as usize;
                    if g.water_top[i] != NO_WATER
                        || inp.building_top[k] != i16::MIN
                        || !pit_classes.contains(&g.class[i])
                    {
                        continue;
                    }
                    pits.insert((lx as u16, lz as u16), g.ground[i]);
                }
            }
        }
    }

    let mut blocks: Vec<(u16, i16, u16, u16)> = Vec::with_capacity(vox.len() + pits.len());
    for (&(x, z), &y) in &pits {
        blocks.push((x, y, z, pit));
    }
    stats.pit_blocks = pits.len();
    for (&(x, y, z), &(kind, b)) in &vox {
        match kind {
            Kind::Log => stats.logs += 1,
            Kind::Leaves => stats.leaves += 1,
        }
        blocks.push((x, y, z, b));
    }
    Ok(VegLayer { palette, blocks, stats })
}

/// Places one voxel (world coordinates) if it lies in the tile and passes the clipping
/// rules.
#[allow(clippy::too_many_arguments)]
fn put(
    vox: &mut BTreeMap<(u16, i16, u16), (Kind, u16)>,
    stats: &mut VegStats,
    inp: &VegInputs,
    clearance: &[u8],
    (bx, bz): (i32, i32),
    (x, y, z): (i32, i32, i32),
    kind: Kind,
    block: u16,
) {
    let size = TILE_SIZE;
    let (lx, lz) = (x - bx, z - bz);
    if !(0..size).contains(&lx) || !(0..size).contains(&lz) {
        return;
    }
    if y >= hhmc_core::voxel::MAX_Y_EXCL {
        return;
    }
    let g = inp.ground;
    let i = g.idx(lx as usize, lz as usize);
    let ground = i32::from(g.ground[i]);
    if y <= ground {
        return;
    }
    if inp.building_top[lz as usize * size as usize + lx as usize] >= y as i16 {
        stats.clipped_building += 1;
        return;
    }
    if g.water_top[i] != NO_WATER && y <= i32::from(g.water_top[i]) {
        stats.clipped_water += 1;
        return;
    }
    if kind == Kind::Leaves
        && (clearance.contains(&g.class[i]) || inp.track_corridor.contains(&(x, z)))
        && y < ground + inp.cfg.conflicts.clearance_blocks
    {
        stats.clipped_clearance += 1;
        return;
    }
    let key = (lx as u16, y as i16, lz as u16);
    match vox.get(&key) {
        Some(&(k, _)) if k > kind => {}
        _ => {
            vox.insert(key, (kind, block));
        }
    }
}

fn hash_str(s: &str) -> u64 {
    // FNV-1a.
    s.bytes()
        .fold(0xcbf2_9ce4_8422_2325u64, |h, b| (h ^ u64::from(b)).wrapping_mul(0x100_0000_01b3))
}

/// Deterministic value in [0, 1) for a tree and a world voxel (splitmix64).
fn noise(seed: u64, x: i32, y: i32, z: i32) -> f32 {
    let mut v = seed
        ^ (x as u32 as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15)
        ^ (y as u32 as u64).wrapping_mul(0xc2b2_ae3d_27d4_eb4f)
        ^ (z as u32 as u64).wrapping_mul(0x1656_67b1_9e37_79f9);
    v = (v ^ (v >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    v = (v ^ (v >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    v ^= v >> 31;
    (v >> 40) as f32 / (1u64 << 24) as f32
}

#[cfg(test)]
mod tests {
    use super::*;
    use hhmc_core::ground::GroundStats;
    use hhmc_core::{TILE_BUFFER, TileKey};

    fn configs() -> (VegetationConfig, BlocksConfig) {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config");
        (
            hhmc_core::config::load_toml(&root.join("vegetation.toml")).unwrap(),
            hhmc_core::config::load_toml(&root.join("blocks.toml")).unwrap(),
        )
    }

    const ORIGIN: Origin = Origin { e0: 500_000, n0: 6_000_000 };

    /// Flat ground at y = 10, class `grass` (index 5), tile (0, 0).
    fn flat() -> GroundModel {
        let w = TILE_SIZE as usize + 2 * TILE_BUFFER as usize;
        GroundModel {
            key: TileKey::new(0, 0),
            min_e: ORIGIN.e0 as f64 - TILE_BUFFER,
            max_n: ORIGIN.n0 as f64 + TILE_BUFFER,
            w,
            h: w,
            ground: vec![10; w * w],
            class: vec![5; w * w],
            water_top: vec![NO_WATER; w * w],
            body: vec![0; w * w],
            stats: GroundStats::default(),
        }
    }

    fn tree(id: &str, x: f64, z: f64, h: f32, crown: f32, source: TreeSource) -> Tree {
        Tree {
            id: id.into(),
            source,
            e: ORIGIN.e0 as f64 + x,
            n: ORIGIN.n0 as f64 - z,
            genus: "Quercus".into(),
            height_m: h,
            height_source: hhmc_core::trees::HeightSource::Als,
            crown_m: crown,
            girth_cm: Some(100.0),
            snapped_m: 0.0,
        }
    }

    fn run(g: &GroundModel, trees: TileTrees, tops: &[i16]) -> VegLayer {
        let (veg, blocks) = configs();
        vegetation(&VegInputs {
            trees: &trees,
            cfg: &veg,
            blocks: &blocks,
            origin: &ORIGIN,
            ground: g,
            building_top: tops,
            track_corridor: &Default::default(),
            stands_on: &Default::default(),
        })
        .unwrap()
    }

    fn name(l: &VegLayer, b: &(u16, i16, u16, u16)) -> String {
        l.palette[b.3 as usize].name.to_string()
    }

    #[test]
    fn trunk_crown_and_pit() {
        let g = flat();
        let none = vec![i16::MIN; 512 * 512];
        let tt = TileTrees {
            own: vec![tree("street:1", 100.5, 100.5, 12.0, 8.0, TreeSource::Street)],
            ..Default::default()
        };
        let mut road = g.clone();
        road.class.iter_mut().for_each(|c| *c = 2);
        let l = run(&road, tt.clone(), &none);
        let at = |x: u16, y: i16, z: u16| l.blocks.iter().find(|b| (b.0, b.1, b.2) == (x, y, z));
        // Trunk on the ground, crown top at ground + height, pit around the trunk.
        assert_eq!(name(&l, at(100, 11, 100).unwrap()), "minecraft:oak_log");
        assert_eq!(name(&l, at(100, 22, 100).unwrap()), "minecraft:oak_leaves");
        assert!(at(100, 23, 100).is_none());
        assert_eq!(name(&l, at(101, 10, 99).unwrap()), "minecraft:rooted_dirt");
        assert_eq!(l.stats.pit_blocks, 9);
        // Leaves over the road keep the clearance.
        assert!(
            l.blocks.iter().filter(|b| name(&l, b).ends_with("_leaves")).all(|b| b.1 >= 10 + 5)
        );
        // Deterministic.
        assert_eq!(run(&road, tt, &none).blocks, l.blocks);
    }

    #[test]
    fn clipped_by_buildings_and_water() {
        let mut g = flat();
        let mut tops = vec![i16::MIN; 512 * 512];
        // A building east of the trunk up to y = 30, water south of it up to y = 17 (the
        // crown starts at y = 15).
        for z in 90..111 {
            for x in 103..111 {
                tops[z * 512 + x] = 30;
            }
        }
        for z in 103..111 {
            for x in 90..103 {
                let i = g.idx(x, z);
                g.water_top[i] = 17;
            }
        }
        let tt = TileTrees {
            own: vec![tree("canopy:1", 100.5, 100.5, 14.0, 10.0, TreeSource::Canopy)],
            ..Default::default()
        };
        let l = run(&g, tt, &tops);
        assert!(l.stats.clipped_building > 0 && l.stats.clipped_water > 0, "{:?}", l.stats);
        for b in &l.blocks {
            let (x, y, z) = (b.0 as usize, i32::from(b.1), b.2 as usize);
            assert!(tops[z * 512 + x] < y as i16, "voxel in building at {x} {y} {z}");
            let i = g.idx(x, z);
            assert!(g.water_top[i] == NO_WATER || y > i32::from(g.water_top[i]));
        }
        // Canopy trees get no pit.
        assert_eq!(l.stats.pit_blocks, 0);
    }

    #[test]
    fn crowns_cross_tile_edges() {
        // A tree on the east edge of tile (0, 0) seen from tile (1, 0) as a neighbor's tree:
        // the two halves fit together.
        let t = tree("street:2", 510.5, 200.5, 10.0, 10.0, TreeSource::Street);
        let none = vec![i16::MIN; 512 * 512];
        let a = run(&flat(), TileTrees { own: vec![t.clone()], ..Default::default() }, &none);
        let mut g1 = flat();
        g1.key = TileKey::new(1, 0);
        let b = run(&g1, TileTrees { near: vec![t], ..Default::default() }, &none);
        let leaves = |l: &VegLayer, dx: i32| -> Vec<(i32, i16, u16)> {
            l.blocks
                .iter()
                .filter(|v| name(l, v).ends_with("_leaves"))
                .map(|v| (i32::from(v.0) + dx, v.1, v.2))
                .collect()
        };
        let (la, lb) = (leaves(&a, 0), leaves(&b, 512));
        assert!(!la.is_empty() && !lb.is_empty());
        // Mirror image around the trunk column x = 510 (+0.5): the crown is symmetric
        // apart from noise, so compare footprints' extents.
        let max_x = lb.iter().map(|v| v.0).max().unwrap();
        let min_x = la.iter().map(|v| v.0).min().unwrap();
        assert!((max_x - 510) - (510 - min_x) <= 1);
    }
}
