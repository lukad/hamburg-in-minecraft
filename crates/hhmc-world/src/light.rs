//! Approximate sky light for the written chunks' `SkyLight` arrays.
//!
//! The game relights every chunk on load (`isLightOn` is not written), so this light is
//! never used in-game; it is for tools that read region files directly, such as Voxy's
//! world import, which takes missing light as darkness. The world has no light-emitting
//! blocks, so there is no block light.
//!
//! Rules (vanilla-like): full blocks stop light and hold 0; air, glass, bars and rails let
//! it pass; leaves and water dim it by 1 per block; slabs and stairs take the light from
//! above (or beside) but pass none on. Light falls straight down from the sky at 15, then
//! spreads to the six neighbors, losing 1 per block (under bridges and eaves, into
//! buildings through their windows). Light does not cross the tile's edge.

use std::collections::VecDeque;

use hhmc_core::voxel::{CHUNKS_PER_SIDE, MAX_Y_EXCL, MIN_Y, SECTIONS};
use hhmc_core::{BlockState, TILE_SIZE, VoxelTile};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Opaque,
    Clear,
    /// Dims light by 1 per block.
    Dim,
    /// Takes light but passes none on.
    Partial,
}

fn kind(s: &BlockState) -> Kind {
    let n = s.name.strip_prefix("minecraft:").unwrap_or(&s.name);
    if n.ends_with("air") {
        Kind::Clear
    } else if n == "water" || n.ends_with("_leaves") {
        Kind::Dim
    } else if n.ends_with("_slab") || n.ends_with("_stairs") {
        Kind::Partial
    } else if n.ends_with("_pane")
        || (n.ends_with("glass") && n != "tinted_glass")
        || n.ends_with("_bars")
        || n.ends_with("rail")
        || n.ends_with("_fence")
        || n.ends_with("_wall")
        || n.ends_with("_carpet")
        || n.ends_with("_trapdoor")
        || n.ends_with("_door")
    {
        Kind::Clear
    } else {
        Kind::Opaque
    }
}

/// Sky light of every block of a tile, 0–15.
pub struct SkyLight {
    data: Vec<u8>,
}

const SIZE: usize = TILE_SIZE as usize;
const HEIGHT: usize = (MAX_Y_EXCL - MIN_Y) as usize;

fn idx(x: usize, y: usize, z: usize) -> usize {
    (y * SIZE + z) * SIZE + x
}

impl SkyLight {
    pub fn of(tile: &VoxelTile) -> Self {
        let kinds: Vec<Kind> = tile.palette.states().iter().map(kind).collect();
        let k = |x: usize, y: usize, z: usize| kinds[tile.get(x, MIN_Y + y as i32, z) as usize];
        let mut data = vec![0u8; SIZE * SIZE * HEIGHT];
        // Straight down from the sky.
        for z in 0..SIZE {
            for x in 0..SIZE {
                let mut level = 15u8;
                for y in (0..HEIGHT).rev() {
                    match k(x, y, z) {
                        Kind::Opaque => break,
                        Kind::Clear => data[idx(x, y, z)] = level,
                        Kind::Dim => {
                            level = level.saturating_sub(1);
                            data[idx(x, y, z)] = level;
                        }
                        Kind::Partial => {
                            data[idx(x, y, z)] = level;
                            break;
                        }
                    }
                    if level == 0 {
                        break;
                    }
                }
            }
        }
        // Spread: from every cell that can light a darker neighbor.
        let neighbors = |i: usize| {
            let (x, z, y) = (i % SIZE, (i / SIZE) % SIZE, i / (SIZE * SIZE));
            [
                (x > 0).then(|| i - 1),
                (x + 1 < SIZE).then(|| i + 1),
                (z > 0).then(|| i - SIZE),
                (z + 1 < SIZE).then(|| i + SIZE),
                (y > 0).then(|| i - SIZE * SIZE),
                (y + 1 < HEIGHT).then(|| i + SIZE * SIZE),
            ]
        };
        let kind_at = |i: usize| {
            let (x, z, y) = (i % SIZE, (i / SIZE) % SIZE, i / (SIZE * SIZE));
            k(x, y, z)
        };
        let mut q: VecDeque<usize> = VecDeque::new();
        for i in 0..data.len() {
            let v = data[i];
            if v < 2 {
                continue;
            }
            let darker = neighbors(i)
                .into_iter()
                .flatten()
                .any(|n| data[n] + 1 < v && kind_at(n) != Kind::Opaque);
            if darker && kind_at(i) != Kind::Partial {
                q.push_back(i);
            }
        }
        while let Some(i) = q.pop_front() {
            let v = data[i];
            for n in neighbors(i).into_iter().flatten() {
                let kn = kind_at(n);
                let new = match kn {
                    Kind::Opaque => continue,
                    Kind::Dim => v.saturating_sub(2),
                    Kind::Clear | Kind::Partial => v.saturating_sub(1),
                };
                if new > data[n] {
                    data[n] = new;
                    if kn != Kind::Partial && new > 1 {
                        q.push_back(n);
                    }
                }
            }
        }
        Self { data }
    }

    /// The `SkyLight` nibble array of section `s` of chunk `(cx, cz)` (index
    /// `y·256 + z·16 + x`, low nibble first), or `None` where it is all dark.
    pub fn section(&self, cx: usize, cz: usize, s: usize) -> Option<Vec<i8>> {
        debug_assert!(cx < CHUNKS_PER_SIDE && cz < CHUNKS_PER_SIDE && s < SECTIONS);
        let mut out = vec![0u8; 2048];
        let mut any = false;
        for ly in 0..16 {
            for lz in 0..16 {
                for lx in 0..16 {
                    let v = self.data[idx(cx * 16 + lx, s * 16 + ly, cz * 16 + lz)];
                    if v != 0 {
                        any = true;
                        let j = ly * 256 + lz * 16 + lx;
                        out[j / 2] |= if j % 2 == 0 { v } else { v << 4 };
                    }
                }
            }
        }
        any.then(|| out.into_iter().map(|b| b as i8).collect())
    }

    /// Sky light at a tile-local block (for tests).
    pub fn get(&self, x: usize, y: i32, z: usize) -> u8 {
        self.data[idx(x, (y - MIN_Y) as usize, z)]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hhmc_core::TileKey;

    #[test]
    fn sky_falls_and_spreads_under_a_roof() {
        let mut t = VoxelTile::new(TileKey::new(0, 0), "minecraft:plains");
        let stone = t.intern(&BlockState::new("stone"));
        let glass = t.intern(&BlockState::new("glass"));
        let water = t.intern(&BlockState::new("water"));
        for z in 0..40 {
            for x in 0..40 {
                t.set(x, 0, z, stone);
            }
        }
        // A 9 × 9 roof at y 4 over (10..19, 10..19).
        for z in 10..19 {
            for x in 10..19 {
                t.set(x, 4, z, stone);
            }
        }
        t.set(30, 1, 30, water);
        t.set(30, 2, 30, glass);
        let l = SkyLight::of(&t);
        assert_eq!(l.get(5, 1, 5), 15, "open sky");
        assert_eq!(l.get(5, 0, 5), 0, "inside stone");
        assert_eq!(l.get(10, 2, 14), 14, "one block under the roof edge");
        assert_eq!(l.get(14, 2, 14), 10, "roof center: 5 blocks from the edge");
        assert_eq!(l.get(30, 2, 30), 15, "glass passes light");
        assert_eq!(l.get(30, 1, 30), 14, "water dims by 1");
        // Packed: low nibble first.
        let sec = l.section(0, 0, (-MIN_Y / 16) as usize).unwrap();
        let j = 256 + 5 * 16 + 5; // y 1, z 5, x 5
        assert_eq!((sec[j / 2] as u8 >> 4) & 15, 15);
        assert!(l.section(0, 0, 0).is_none(), "bottom section is dark");
    }
}
