//! Compose: turns stage results into a `VoxelTile`. Layers are merged in the order
//! ground → water → infrastructure → vegetation → buildings → landmarks.

use anyhow::Result;
use hhmc_core::ground::{GroundModel, NO_WATER};
use hhmc_core::landcfg::BlocksConfig;
use hhmc_core::voxel::{MAX_Y_EXCL, MIN_Y};
use hhmc_core::{BlockState, TILE_SIZE, VoxelTile};

/// Bump when the composition logic changes.
pub const COMPOSE_VERSION: u32 = 4;

/// Water source block; `level=0` is a still source.
pub fn water_state() -> BlockState {
    BlockState::new("minecraft:water").with("level", "0")
}

/// Ground columns (bedrock, deep stone, subsoil, top) and water up to the body's level.
pub fn compose_ground(g: &GroundModel, blocks: &BlocksConfig) -> Result<VoxelTile> {
    let mut t = VoxelTile::new(g.key, &blocks.classes[0].biome);
    let floor = t.intern(&BlockState::parse(&blocks.floor).map_err(anyhow::Error::msg)?);
    let deep = t.intern(&BlockState::parse(&blocks.deep).map_err(anyhow::Error::msg)?);
    let water = t.intern(&water_state());
    let mut tops = Vec::new();
    let mut subs = Vec::new();
    let mut biomes = Vec::new();
    for c in &blocks.classes {
        tops.push(t.intern(&BlockState::parse(&c.top).map_err(anyhow::Error::msg)?));
        subs.push(t.intern(&BlockState::parse(&c.subsoil).map_err(anyhow::Error::msg)?));
        biomes.push(t.biome_index(&c.biome));
    }
    let size = TILE_SIZE as usize;
    for z in 0..size {
        for x in 0..size {
            let i = g.idx(x, z);
            let gy = i32::from(g.ground[i]).clamp(MIN_Y + 1, MAX_Y_EXCL - 1);
            let c = (g.class[i] as usize).min(blocks.classes.len() - 1);
            t.set(x, MIN_Y, z, floor);
            t.fill_column(x, z, MIN_Y + 1, gy - blocks.subsoil_depth - 1, deep);
            t.fill_column(x, z, (gy - blocks.subsoil_depth).max(MIN_Y + 1), gy - 1, subs[c]);
            t.set(x, gy, z, tops[c]);
            if g.water_top[i] != NO_WATER {
                t.fill_column(x, z, gy + 1, i32::from(g.water_top[i]), water);
            }
        }
    }
    // Biomes per 4 × 4 column cell, from the cell's center column, for the whole height.
    for cz in 0..size / 4 {
        for cx in 0..size / 4 {
            let i = g.idx(cx * 4 + 2, cz * 4 + 2);
            let b = biomes[(g.class[i] as usize).min(biomes.len() - 1)];
            if b == 0 {
                continue;
            }
            for y in (MIN_Y..MAX_Y_EXCL).step_by(4) {
                t.set_biome(cx * 4, y, cz * 4, b);
            }
        }
    }
    Ok(t)
}

/// Applies a layer: clears its columns' ranges to air (building interiors, removing
/// terrain, water and trees), then places its blocks. Layers are applied in a fixed
/// order (vegetation, then buildings), so later layers win.
pub fn apply_layer(
    t: &mut VoxelTile,
    palette: &[BlockState],
    clears: &[(u16, u16, i16, i16)],
    blocks: &[(u16, i16, u16, u16)],
) {
    for &(x, z, y0, y1) in clears {
        t.fill_column(x as usize, z as usize, i32::from(y0), i32::from(y1), hhmc_core::AIR);
    }
    let ids: Vec<_> = palette.iter().map(|s| t.intern(s)).collect();
    for &(x, y, z, i) in blocks {
        t.set(x as usize, i32::from(y), z as usize, ids[i as usize]);
    }
}
