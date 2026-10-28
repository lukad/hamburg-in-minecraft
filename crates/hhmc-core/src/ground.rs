//! The ground stage's result for one tile, shared with compose and later stages.

use serde::{Deserialize, Serialize};

use crate::TileKey;

/// Marks a column without water.
pub const NO_WATER: i16 = i16::MIN;

/// Ground model on the tile's read box (tile plus buffer), 1 m cells, row 0 north.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GroundModel {
    pub key: TileKey,
    pub min_e: f64,
    pub max_n: f64,
    pub w: usize,
    pub h: usize,
    /// y of the topmost ground block (bed for water columns).
    pub ground: Vec<i16>,
    /// Surface class index (config/blocks.toml).
    pub class: Vec<u8>,
    /// y of the topmost water block, or [`NO_WATER`].
    pub water_top: Vec<i16>,
    /// Water body index (data/prepared/water/bodies.json), 0 = dry.
    pub body: Vec<u16>,
    pub stats: GroundStats,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct GroundStats {
    /// Heights outside the clamp range (inside the tile proper).
    pub clamped: usize,
    /// Water columns whose bed is synthetic or blended (no DGM-W).
    pub synthetic_bed: usize,
    /// Land columns without DGM1, filled from DGM-W or the nearest neighbor.
    pub filled_land: usize,
    /// Columns turned into lock gates between water bodies of different levels.
    pub lock_gates: usize,
}

impl GroundModel {
    /// Index of tile-local block column `(x, z)` (0..512) in the buffered grid.
    #[inline]
    pub fn idx(&self, x: usize, z: usize) -> usize {
        let b = (self.w - crate::TILE_SIZE as usize) / 2;
        (z + b) * self.w + x + b
    }

    /// Index of tile-local column `(x, z)`, which may lie in the buffer (negative or
    /// ≥ 512), or `None` outside the grid.
    #[inline]
    pub fn idx_buffered(&self, x: i32, z: i32) -> Option<usize> {
        let b = ((self.w - crate::TILE_SIZE as usize) / 2) as i32;
        let (c, r) = (x + b, z + b);
        (c >= 0 && r >= 0 && (c as usize) < self.w && (r as usize) < self.h)
            .then(|| r as usize * self.w + c as usize)
    }
}
