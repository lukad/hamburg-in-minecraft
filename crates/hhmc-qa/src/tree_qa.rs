//! Tree checks on the written region files:
//! - every cadaster tree of the tile is either placed (its trunk's lowest log stands on
//!   the ground, or on the deck of a bridge abutment) or logged as a conflict by prep, and nothing else is placed;
//! - no tree voxel (log or leaves of a configured family) inside a building, in water, or
//!   on a road surface (below the clearance over a road column whose top block is the
//!   class's surface, i.e. not a tree pit, or within the corridor of a railway track).

use std::collections::{BTreeSet, HashSet};

use hhmc_core::config::Origin;
use hhmc_core::ground::{GroundModel, NO_WATER};
use hhmc_core::landcfg::BlocksConfig;
use hhmc_core::trees::{CadasterTree, TileTrees, TreeSource, VegetationConfig};
use hhmc_core::voxel::{MAX_Y_EXCL, MIN_Y};
use hhmc_core::{TILE_SIZE, TileKey, VoxelTile};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct TreeQaResult {
    /// Cadaster trees with their trunk in the tile, as published.
    pub cadaster: usize,
    /// Of these: placed by prep, and logged as conflicts.
    pub placed: usize,
    pub conflicts: usize,
    /// Cadaster ids neither placed nor logged, or placed / logged without being in the
    /// cadaster (up to 20).
    pub unaccounted: Vec<String>,
    pub unaccounted_count: usize,
    pub canopy: usize,
    /// Placed trees (all sources) without a log on the ground at the trunk (up to 20 ids).
    pub trunks_missing: Vec<String>,
    pub trunks_missing_count: usize,
    pub tree_voxels: usize,
    pub in_building: usize,
    pub in_water: usize,
    pub on_road: usize,
    /// World positions of the first offending voxels.
    pub samples: Vec<[i32; 3]>,
    pub pass: bool,
}

/// Palette ids of the tile's tree blocks (logs and leaves of the configured families).
pub fn tree_block_ids(t: &VoxelTile, veg: &VegetationConfig) -> HashSet<u16> {
    let names: HashSet<String> = veg
        .family
        .values()
        .flat_map(|f| [f.log.clone(), f.leaves.clone()])
        .map(|s| s.split('[').next().unwrap_or(&s).to_string())
        .collect();
    t.palette
        .states()
        .iter()
        .enumerate()
        .filter(|(_, s)| names.contains(&*s.name))
        .map(|(i, _)| i as u16)
        .collect()
}

pub struct TreeCheckInputs<'a> {
    pub key: TileKey,
    pub origin: &'a Origin,
    pub tile: &'a VoxelTile,
    pub ground: &'a GroundModel,
    /// Highest building block per tile column, `i16::MIN` if none.
    pub building_top: &'a [i16],
    pub trees: &'a TileTrees,
    /// The published cadasters (all trees; filtered to the tile here).
    pub cadaster: &'a [CadasterTree],
    pub veg: &'a VegetationConfig,
    pub blocks: &'a BlocksConfig,
    /// World columns along railway tracks: tree voxels keep the clearance there.
    pub track_corridor: &'a std::collections::HashSet<(i32, i32)>,
    /// Deck tops trees stand on (bridge abutments), by world column.
    pub stands_on: &'a std::collections::HashMap<(i32, i32), i32>,
}

pub fn check_trees(inp: &TreeCheckInputs) -> TreeQaResult {
    let (t, g) = (inp.tile, inp.ground);
    let size = TILE_SIZE as usize;
    let (bx, bz) = inp.key.min_block();
    let mut r = TreeQaResult::default();
    let in_tile = |e: f64, n: f64| TileKey::from_utm(inp.origin, e, n) == inp.key;

    // Bookkeeping: cadaster = placed + conflicts, id by id.
    let cad: BTreeSet<&str> =
        inp.cadaster.iter().filter(|c| in_tile(c.e, c.n)).map(|c| c.id.as_str()).collect();
    // Placed trees are listed under their (possibly snapped) trunk, which may have moved
    // into a neighboring tile's `near` list; look them up by id.
    let placed: BTreeSet<&str> = inp
        .trees
        .own
        .iter()
        .chain(&inp.trees.near)
        .filter(|tr| tr.source != TreeSource::Canopy && cad.contains(tr.id.as_str()))
        .map(|tr| tr.id.as_str())
        .collect();
    let conflicts: BTreeSet<&str> = inp.trees.conflicts.iter().map(|c| c.id.as_str()).collect();
    let all: HashSet<&str> = inp.cadaster.iter().map(|c| c.id.as_str()).collect();
    r.cadaster = cad.len();
    r.placed = placed.len();
    r.conflicts = conflicts.len();
    let mut odd: Vec<&str> = cad
        .iter()
        .filter(|id| !placed.contains(*id) && !conflicts.contains(*id))
        .copied()
        .collect();
    // Placed or logged trees that the cadaster does not have.
    odd.extend(conflicts.iter().filter(|id| !cad.contains(*id)));
    odd.extend(
        inp.trees
            .own
            .iter()
            .filter(|tr| tr.source != TreeSource::Canopy && !all.contains(tr.id.as_str()))
            .map(|tr| tr.id.as_str()),
    );
    r.unaccounted_count = odd.len();
    r.unaccounted = odd.iter().take(20).map(|s| s.to_string()).collect();
    r.canopy = inp.trees.own.iter().filter(|tr| tr.source == TreeSource::Canopy).count();

    let tree_ids = tree_block_ids(t, inp.veg);
    let logs: HashSet<u16> = {
        let names: HashSet<String> = inp.veg.family.values().map(|f| f.log.clone()).collect();
        tree_ids.iter().copied().filter(|&i| names.contains(&*t.palette.get(i).name)).collect()
    };
    // Trunks: a log right above the ground at the trunk column, or at one of the 2 × 2
    // cells around it for big trunks.
    for tr in &inp.trees.own {
        let (wx, wz) = inp.origin.block_at(tr.e, tr.n);
        // Park trees are not placed on railway tracks.
        if tr.source == TreeSource::Canopy && inp.track_corridor.contains(&(wx, wz)) {
            continue;
        }
        let (lx, lz) = (wx - bx, wz - bz);
        let on = inp.stands_on.get(&(wx, wz)).copied().unwrap_or(i32::MIN);
        let found = [(0, 0), (-1, 0), (0, -1), (-1, -1), (1, 0), (0, 1), (1, 1)]
            .iter()
            .take(if tr.girth_cm.is_some_and(|c| c >= inp.veg.tree.big_trunk_girth_cm) {
                7
            } else {
                1
            })
            .any(|(dx, dz)| {
                let (x, z) = (lx + dx, lz + dz);
                if !(0..TILE_SIZE).contains(&x) || !(0..TILE_SIZE).contains(&z) {
                    return false;
                }
                let gy = i32::from(g.ground[g.idx(x as usize, z as usize)]).max(on);
                logs.contains(&t.get(x as usize, gy + 1, z as usize))
            });
        if !found {
            r.trunks_missing_count += 1;
            if r.trunks_missing.len() < 20 {
                r.trunks_missing.push(tr.id.clone());
            }
        }
    }

    // Voxels.
    let class_of = |name: &str| inp.blocks.classes.iter().position(|c| c.id == name);
    let road: Vec<(u8, String)> = inp
        .veg
        .conflicts
        .clearance_classes
        .iter()
        .filter_map(|n| class_of(n).map(|i| (i as u8, inp.blocks.classes[i].top.clone())))
        .collect();
    for z in 0..size {
        for x in 0..size {
            let i = g.idx(x, z);
            let gy = i32::from(g.ground[i]);
            let road_surface = road.iter().any(|(c, top)| {
                g.class[i] == *c
                    && *t.palette.get(t.get(x, gy, z)).name == *top.split('[').next().unwrap_or(top)
            });
            let in_corridor = inp.track_corridor.contains(&(bx + x as i32, bz + z as i32));
            let btop = i32::from(inp.building_top[z * size + x]);
            for y in MIN_Y..MAX_Y_EXCL {
                if !tree_ids.contains(&t.get(x, y, z)) {
                    continue;
                }
                r.tree_voxels += 1;
                let bad = if y <= btop {
                    r.in_building += 1;
                    true
                } else if g.water_top[i] != NO_WATER && y <= i32::from(g.water_top[i]) {
                    r.in_water += 1;
                    true
                } else if (road_surface || (in_corridor && !logs.contains(&t.get(x, y, z))))
                    && y < gy + inp.veg.conflicts.clearance_blocks
                {
                    r.on_road += 1;
                    true
                } else {
                    false
                };
                if bad && r.samples.len() < 20 {
                    r.samples.push([bx + x as i32, y, bz + z as i32]);
                }
            }
        }
    }
    r.pass = r.unaccounted_count == 0
        && r.trunks_missing_count == 0
        && r.in_building == 0
        && r.in_water == 0
        && r.on_road == 0;
    r
}
