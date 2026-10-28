//! Shared foundation of the `hhmc` pipeline: tile grid and coordinate mapping,
//! configuration files and the area definitions.

pub mod area;
pub mod boundary;
pub mod cache;
pub mod config;
pub mod geom;
pub mod ground;
pub mod infra;
pub mod landcfg;
pub mod proj;
pub mod raster;
pub mod tile;
pub mod trees;
pub mod vector;
pub mod voxel;

pub use area::{Area, Areas};
pub use config::{Paths, WorldConfig};
pub use geom::Bbox;
pub use tile::{TILE_BUFFER, TILE_SIZE, TileKey};
pub use voxel::{AIR, BlockId, BlockState, Palette, VoxelTile};
