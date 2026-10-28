//! Chunk NBT structures for the pinned version (26.3, DataVersion 5023).
//!
//! Keys from `SerializableChunkData.write`/`parse` in the 26.3 server jar:
//! `DataVersion`, `xPos`, `yPos`, `zPos`, `Status`, `LastUpdate`, `InhabitedTime`,
//! `sections[{Y, block_states{palette, data}, biomes{palette, data}}]`.
//!
//! Deliberately omitted:
//! - `isLightOn`, `BlockLight`: absent → the server relights the chunk on load. `SkyLight`
//!   is written (approximate, see `light`) for tools that import region files directly,
//!   such as Voxy; the server ignores stored light without `isLightOn`.
//! - `Heightmaps`: absent types are recomputed on load (`Heightmap.primeHeightmaps`).
//!
//! Block-state palette entries changed in 26.3 (snapshot 7) from `{Name, Properties}` to
//! `{id, properties}`; the codec also accepts a bare block-name string, and the server
//! itself saves default states as strings (wrapped as `{"": name}` in mixed lists).

use std::collections::BTreeMap;

use fastnbt::LongArray;
use serde::{Deserialize, Serialize};

/// `Status` of a fully generated chunk; anything else makes the server continue generation.
pub const STATUS_FULL: &str = "minecraft:full";

#[derive(Debug, Serialize, Deserialize)]
pub struct ChunkNbt {
    #[serde(rename = "DataVersion")]
    pub data_version: i32,
    #[serde(rename = "xPos")]
    pub x_pos: i32,
    #[serde(rename = "yPos")]
    pub y_pos: i32,
    #[serde(rename = "zPos")]
    pub z_pos: i32,
    #[serde(rename = "Status")]
    pub status: String,
    #[serde(rename = "LastUpdate", default)]
    pub last_update: i64,
    #[serde(rename = "InhabitedTime", default)]
    pub inhabited_time: i64,
    #[serde(default)]
    pub sections: Vec<SectionNbt>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SectionNbt {
    #[serde(rename = "Y")]
    pub y: i8,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub block_states: Option<Paletted<BlockStateNbt>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub biomes: Option<Paletted<String>>,
    /// Approximate sky light (see `light`), for tools that read region files; the game
    /// ignores it because `isLightOn` is absent.
    #[serde(rename = "SkyLight", default, skip_serializing_if = "Option::is_none")]
    pub sky_light: Option<fastnbt::ByteArray>,
}

/// A paletted container: `data` is absent when the palette has a single entry.
#[derive(Debug, Serialize, Deserialize)]
pub struct Paletted<T> {
    pub palette: Vec<T>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<LongArray>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum BlockStateNbt {
    Full {
        id: String,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        properties: BTreeMap<String, String>,
    },
    /// Block name only: the block's default state. Accepted on read, never written.
    Name(String),
    /// The server's own encoding of a bare name inside a list that also holds compounds
    /// (NBT lists are homogeneous): `{"": "minecraft:x"}`. Accepted on read, never written.
    Wrapped {
        #[serde(rename = "")]
        name: String,
    },
}
