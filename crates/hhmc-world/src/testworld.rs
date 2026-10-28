//! The test world: a 16-block checkerboard, marker blocks in every chunk corner,
//! a full-height column per tile and a few block states with properties and biomes.

use hhmc_core::voxel::MIN_Y;
use hhmc_core::{BlockState, TileKey, VoxelTile};

/// A block the server must report at a world position.
#[derive(Clone, Debug)]
pub struct Expect {
    pub x: i32,
    pub y: i32,
    pub z: i32,
    pub state: BlockState,
}

const CORNERS: [(usize, usize, &str); 4] =
    [(0, 0, "red_wool"), (15, 0, "blue_wool"), (0, 15, "lime_wool"), (15, 15, "yellow_wool")];

fn stairs() -> BlockState {
    BlockState::new("oak_stairs")
        .with("facing", "east")
        .with("half", "top")
        .with("shape", "straight")
        .with("waterlogged", "false")
}

/// Builds test tile `key`; returns it plus the assertions for that tile.
pub fn test_tile(key: TileKey) -> (VoxelTile, Vec<Expect>) {
    let mut t = VoxelTile::new(key, "minecraft:plains");
    let (bx, bz) = key.min_block();
    let bedrock = t.intern(&BlockState::new("bedrock"));
    let white = t.intern(&BlockState::new("white_concrete"));
    let black = t.intern(&BlockState::new("black_concrete"));
    let river = t.biome_index("minecraft:river");
    let mut expect = Vec::new();

    for z in 0..512usize {
        for x in 0..512usize {
            t.set(x, MIN_Y, z, bedrock);
            // Checkerboard of 16-block squares in world coordinates.
            let wx = bx + x as i32;
            let wz = bz + z as i32;
            let even = (wx.div_euclid(16) + wz.div_euclid(16)).rem_euclid(2) == 0;
            t.set(x, 0, z, if even { white } else { black });
        }
    }
    for cz in 0..32usize {
        for cx in 0..32usize {
            for (dx, dz, name) in CORNERS {
                let (x, z) = (cx * 16 + dx, cz * 16 + dz);
                let s = BlockState::new(name);
                let id = t.intern(&s);
                t.set(x, 1, z, id);
                if (cx + cz) % 7 == 0 {
                    expect.push(Expect { x: bx + x as i32, y: 1, z: bz + z as i32, state: s });
                }
            }
        }
    }
    // Checkerboard samples.
    for (x, z) in [(0usize, 0usize), (16, 0), (17, 17), (300, 450)] {
        let s = t.palette.get(t.get(x, 0, z)).clone();
        expect.push(Expect { x: bx + x as i32, y: 0, z: bz + z as i32, state: s });
    }
    // Full-height column in the tile center: distinct blocks at both ends.
    let (cx, cz) = (256usize, 256usize);
    let iron = t.intern(&BlockState::new("iron_block"));
    t.fill_column(cx, cz, MIN_Y, 319, iron);
    let diamond = BlockState::new("diamond_block");
    let emerald = BlockState::new("emerald_block");
    let (d, e) = (t.intern(&diamond), t.intern(&emerald));
    t.set(cx, MIN_Y, cz, d);
    t.set(cx, 319, cz, e);
    let (wx, wz) = (bx + cx as i32, bz + cz as i32);
    expect.push(Expect { x: wx, y: MIN_Y, z: wz, state: diamond });
    expect.push(Expect { x: wx, y: 319, z: wz, state: emerald });
    for y in [-63, -1, 0, 15, 16, 255, 318] {
        expect.push(Expect { x: wx, y, z: wz, state: BlockState::new("iron_block") });
    }
    // A block state with properties, and a river biome patch around it.
    let st = stairs();
    let sid = t.intern(&st);
    t.set(100, 2, 100, sid);
    expect.push(Expect { x: bx + 100, y: 2, z: bz + 100, state: st });
    for z in 96..112 {
        for x in 96..112 {
            for y in -4..12 {
                t.set_biome(x, y, z, river);
            }
        }
    }
    (t, expect)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chunk::{first_difference, read_tile, tile_to_region};

    #[test]
    fn round_trip_every_block() {
        let key = TileKey::new(-1, 0);
        let (tile, _) = test_tile(key);
        let bytes = tile_to_region(&tile, 5023).unwrap();
        let dir = std::env::temp_dir().join(format!("hhmc-rt-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(key.region_file_name());
        std::fs::write(&path, &bytes).unwrap();
        let back = read_tile(&path, key).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(first_difference(&tile, &back), None);
    }

    /// Reads a region the vanilla server re-saved after `hhmc server-test` and compares it
    /// with the test tile. Run with `HHMC_SERVER_SAVED=<…/r.0.0.mca> cargo test -- --ignored`.
    #[test]
    #[ignore]
    fn server_saved_region_matches() {
        let path = std::env::var("HHMC_SERVER_SAVED").expect("set HHMC_SERVER_SAVED");
        let key = TileKey::new(0, 0);
        let (tile, _) = test_tile(key);
        let back = read_tile(std::path::Path::new(&path), key).unwrap();
        assert_eq!(first_difference(&tile, &back), None);
    }

    #[test]
    fn deterministic_bytes() {
        let (a, _) = test_tile(TileKey::new(0, 0));
        let (b, _) = test_tile(TileKey::new(0, 0));
        assert_eq!(tile_to_region(&a, 5023).unwrap(), tile_to_region(&b, 5023).unwrap());
    }
}
