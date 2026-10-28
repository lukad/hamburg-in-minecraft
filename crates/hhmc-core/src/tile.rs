//! The 512 m tile grid. One tile is exactly one Minecraft region file.
//!
//! World mapping (see `config/world.toml`): `x = E − E0`, `z = N0 − N`, so +z points
//! south. Block `(x, z)` covers `E ∈ [E0 + x, E0 + x + 1)` and `N ∈ (N0 − z − 1, N0 − z]`;
//! its center is `(E0 + x + 0.5, N0 − z − 0.5)`.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::config::Origin;
use crate::geom::Bbox;

/// Edge length of a tile in meters (= blocks). One region is 32 × 32 chunks of 16 blocks.
pub const TILE_SIZE: i32 = 512;

/// Inputs are read with this buffer (meters) around a tile so that objects crossing
/// tile edges are generated identically on both sides.
pub const TILE_BUFFER: f64 = 32.0;

/// Region coordinates `(rx, rz)`; the tile's file is `r.<rx>.<rz>.mca`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(from = "[i32; 2]", into = "[i32; 2]")]
pub struct TileKey {
    pub rx: i32,
    pub rz: i32,
}

impl TileKey {
    pub const fn new(rx: i32, rz: i32) -> Self {
        Self { rx, rz }
    }

    /// Tile containing world block `(x, z)`.
    pub fn from_block(x: i32, z: i32) -> Self {
        Self::new(x.div_euclid(TILE_SIZE), z.div_euclid(TILE_SIZE))
    }

    /// Tile containing the EPSG:25832 point `(e, n)`.
    pub fn from_utm(origin: &Origin, e: f64, n: f64) -> Self {
        let (x, z) = origin.block_at(e, n);
        Self::from_block(x, z)
    }

    /// World block coordinates of the tile's north-west corner block.
    pub fn min_block(&self) -> (i32, i32) {
        (self.rx * TILE_SIZE, self.rz * TILE_SIZE)
    }

    /// Exact footprint of the tile in EPSG:25832.
    pub fn bbox(&self, origin: &Origin) -> Bbox {
        let (x0, z0) = self.min_block();
        let min_e = origin.e0 as f64 + x0 as f64;
        let max_n = origin.n0 as f64 - z0 as f64;
        Bbox::new(min_e, max_n - TILE_SIZE as f64, min_e + TILE_SIZE as f64, max_n)
    }

    /// Footprint plus [`TILE_BUFFER`]: the region whose inputs a tile stage reads.
    pub fn read_bbox(&self, origin: &Origin) -> Bbox {
        self.bbox(origin).buffered(TILE_BUFFER)
    }

    pub fn region_file_name(&self) -> String {
        format!("r.{}.{}.mca", self.rx, self.rz)
    }
}

impl fmt::Display for TileKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{},{}", self.rx, self.rz)
    }
}

impl FromStr for TileKey {
    type Err = String;

    /// Parses `"rx,rz"`.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (a, b) =
            s.split_once(',').ok_or_else(|| format!("tile `{s}` is not of the form rx,rz"))?;
        let p = |v: &str| v.trim().parse::<i32>().map_err(|e| format!("tile `{s}`: {e}"));
        Ok(Self::new(p(a)?, p(b)?))
    }
}

impl From<[i32; 2]> for TileKey {
    fn from(a: [i32; 2]) -> Self {
        Self::new(a[0], a[1])
    }
}

impl From<TileKey> for [i32; 2] {
    fn from(t: TileKey) -> Self {
        [t.rx, t.rz]
    }
}

/// Parses `"rx,rz;rx,rz;…"` as used by `hhmc build --tiles`.
pub fn parse_tile_list(s: &str) -> Result<Vec<TileKey>, String> {
    s.split(';').map(str::trim).filter(|p| !p.is_empty()).map(TileKey::from_str).collect()
}

/// All tiles whose footprint intersects `bbox`, i.e. `bbox` snapped outward to the grid.
pub fn tiles_covering(origin: &Origin, bbox: &Bbox) -> Vec<TileKey> {
    // x grows with E, z grows as N falls.
    let x_min = (bbox.min_e - origin.e0 as f64).floor() as i32;
    let x_max = (bbox.max_e - origin.e0 as f64).ceil() as i32 - 1;
    let z_min = (origin.n0 as f64 - bbox.max_n).floor() as i32;
    let z_max = (origin.n0 as f64 - bbox.min_n).ceil() as i32 - 1;
    let (rx0, rz0) = (x_min.div_euclid(TILE_SIZE), z_min.div_euclid(TILE_SIZE));
    let (rx1, rz1) = (x_max.div_euclid(TILE_SIZE), z_max.div_euclid(TILE_SIZE));
    let mut out = Vec::new();
    for rz in rz0..=rz1 {
        for rx in rx0..=rx1 {
            out.push(TileKey::new(rx, rz));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn origin() -> Origin {
        Origin { e0: 565760, n0: 5934080 }
    }

    #[test]
    fn negative_blocks_floor_to_negative_tiles() {
        assert_eq!(TileKey::from_block(-1, -1), TileKey::new(-1, -1));
        assert_eq!(TileKey::from_block(-512, 511), TileKey::new(-1, 0));
        assert_eq!(TileKey::from_block(512, -513), TileKey::new(1, -2));
    }

    #[test]
    fn bbox_round_trip() {
        let o = origin();
        let t = TileKey::new(-4, 3);
        let b = t.bbox(&o);
        assert_eq!(b.width(), 512.0);
        assert_eq!(b.height(), 512.0);
        // Center of the tile's first block maps back to the tile.
        assert_eq!(TileKey::from_utm(&o, b.min_e + 0.5, b.max_n - 0.5), t);
        // Last block too.
        assert_eq!(TileKey::from_utm(&o, b.max_e - 0.5, b.min_n + 0.5), t);
        assert_eq!(tiles_covering(&o, &b), vec![t]);
    }

    #[test]
    fn covering_snaps_outward() {
        let o = origin();
        let b = Bbox::new(565760.0 - 1.0, 5934080.0 - 513.0, 565760.0 + 1.0, 5934080.0 - 1.0);
        let tiles = tiles_covering(&o, &b);
        assert_eq!(
            tiles,
            vec![TileKey::new(-1, 0), TileKey::new(0, 0), TileKey::new(-1, 1), TileKey::new(0, 1)]
        );
    }

    #[test]
    fn parse_list() {
        assert_eq!(
            parse_tile_list("1,2; -3,-4;").unwrap(),
            vec![TileKey::new(1, 2), TileKey::new(-3, -4)]
        );
        assert!(parse_tile_list("1;2").is_err());
    }
}
