//! Writing `VoxelTile`s as Anvil region files for the pinned Minecraft version, reading
//! them back, and assembling a playable world directory.
//!
//! Format facts were taken from the 26.3 server jar (`SerializableChunkData`,
//! `RegionFile`, `RegionFileVersion`, `SimpleBitStorage`, `PalettedContainer`), which ships
//! unobfuscated, cross-checked against <https://minecraft.wiki/w/Chunk_format> and
//! <https://minecraft.wiki/w/Region_file_format> (retrieved 2026-10-26).

pub mod chunk;
pub mod compose;
pub mod layout;
pub mod level;
pub mod light;
pub mod nbt;
pub mod pack;
pub mod region;
pub mod testworld;

pub use chunk::{read_tile, tile_to_region, write_tile};
pub use layout::WorldLayout;
