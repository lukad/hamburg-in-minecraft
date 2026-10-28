//! `VoxelTile`: a sparse 512 × 384 × 512 block volume laid out like an Anvil region,
//! so writing it is a straight copy (32 × 32 chunks × 24 sections of 16³ blocks).

use std::collections::{BTreeMap, HashMap};

use serde::{Deserialize, Serialize};

use crate::tile::{TILE_SIZE, TileKey};

/// Lowest block y of the overworld.
pub const MIN_Y: i32 = -64;
/// Number of 16-block sections per chunk column (y = −64 … 319).
pub const SECTIONS: usize = 24;
/// One past the highest block y.
pub const MAX_Y_EXCL: i32 = MIN_Y + 16 * SECTIONS as i32;
pub const CHUNKS_PER_SIDE: usize = (TILE_SIZE / 16) as usize;

/// A block state: namespaced id plus sorted properties, e.g.
/// `minecraft:oak_stairs[facing=north,half=bottom]`.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct BlockState {
    pub name: String,
    pub properties: BTreeMap<String, String>,
}

impl BlockState {
    pub fn new(name: &str) -> Self {
        let name = if name.contains(':') { name.to_owned() } else { format!("minecraft:{name}") };
        Self { name, properties: BTreeMap::new() }
    }

    pub fn with(mut self, key: &str, value: &str) -> Self {
        self.properties.insert(key.to_owned(), value.to_owned());
        self
    }

    pub fn air() -> Self {
        Self::new("minecraft:air")
    }

    pub fn is_air(&self) -> bool {
        matches!(&*self.name, "minecraft:air" | "minecraft:cave_air" | "minecraft:void_air")
    }

    /// Parses `name[k=v,…]`.
    pub fn parse(s: &str) -> Result<Self, String> {
        let s = s.trim();
        let (name, props) = match s.split_once('[') {
            Some((n, rest)) => {
                (n, rest.strip_suffix(']').ok_or_else(|| format!("unclosed `[` in `{s}`"))?)
            }
            None => (s, ""),
        };
        let mut b = Self::new(name);
        for kv in props.split(',').filter(|p| !p.is_empty()) {
            let (k, v) = kv.split_once('=').ok_or_else(|| format!("bad property `{kv}`"))?;
            b = b.with(k.trim(), v.trim());
        }
        Ok(b)
    }
}

impl std::fmt::Display for BlockState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.name)?;
        if !self.properties.is_empty() {
            let p: Vec<String> = self.properties.iter().map(|(k, v)| format!("{k}={v}")).collect();
            write!(f, "[{}]", p.join(","))?;
        }
        Ok(())
    }
}

/// Index into a [`Palette`]. Index 0 is always air.
pub type BlockId = u16;
pub const AIR: BlockId = 0;

/// Tile-wide palette of block states.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Palette {
    states: Vec<BlockState>,
    #[serde(skip)]
    index: HashMap<BlockState, BlockId>,
}

impl Default for Palette {
    fn default() -> Self {
        let mut p = Self { states: Vec::new(), index: HashMap::new() };
        p.intern(&BlockState::air());
        p
    }
}

impl Palette {
    pub fn intern(&mut self, s: &BlockState) -> BlockId {
        if let Some(&id) = self.index.get(s) {
            return id;
        }
        let id =
            BlockId::try_from(self.states.len()).expect("more than 65536 block states in a tile");
        self.states.push(s.clone());
        self.index.insert(s.clone(), id);
        id
    }

    pub fn get(&self, id: BlockId) -> &BlockState {
        &self.states[id as usize]
    }

    pub fn len(&self) -> usize {
        self.states.len()
    }

    pub fn is_empty(&self) -> bool {
        self.states.is_empty()
    }

    pub fn states(&self) -> &[BlockState] {
        &self.states
    }

    /// Rebuilds the lookup index after deserialization.
    pub fn reindex(&mut self) {
        self.index =
            self.states.iter().enumerate().map(|(i, s)| (s.clone(), i as BlockId)).collect();
    }
}

/// 16³ blocks in Anvil order: index = `(y * 16 + z) * 16 + x`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Section {
    pub blocks: Box<[BlockId]>,
}

impl Section {
    pub fn filled(id: BlockId) -> Self {
        Self { blocks: vec![id; 4096].into_boxed_slice() }
    }

    #[inline]
    pub fn index(x: usize, y: usize, z: usize) -> usize {
        (y * 16 + z) * 16 + x
    }

    pub fn is_uniform(&self) -> Option<BlockId> {
        let first = self.blocks[0];
        self.blocks.iter().all(|&b| b == first).then_some(first)
    }
}

/// Biome of a section at 4 × 4 × 4 resolution, index = `(y * 4 + z) * 4 + x`.
pub type SectionBiomes = Box<[u8]>;

/// One chunk column: 24 optional sections; `None` means all air.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Chunk {
    pub sections: Vec<Option<Section>>,
    /// Per-section biome indices into [`VoxelTile::biomes`]; `None` means index 0.
    pub biomes: Vec<Option<SectionBiomes>>,
}

impl Chunk {
    fn new() -> Self {
        Self { sections: vec![None; SECTIONS], biomes: vec![None; SECTIONS] }
    }
}

/// One tile = one region. Local coordinates: `x, z ∈ 0..512`, `y ∈ −64..320`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VoxelTile {
    pub key: TileKey,
    pub palette: Palette,
    /// Biome ids such as `minecraft:plains`; index 0 is the default biome.
    pub biomes: Vec<String>,
    /// Row-major by chunk z then chunk x.
    pub chunks: Vec<Chunk>,
}

impl VoxelTile {
    pub fn new(key: TileKey, default_biome: &str) -> Self {
        Self {
            key,
            palette: Palette::default(),
            biomes: vec![default_biome.to_owned()],
            chunks: (0..CHUNKS_PER_SIDE * CHUNKS_PER_SIDE).map(|_| Chunk::new()).collect(),
        }
    }

    #[inline]
    fn locate(x: usize, y: i32, z: usize) -> Option<(usize, usize, usize)> {
        if x >= TILE_SIZE as usize || z >= TILE_SIZE as usize || !(MIN_Y..MAX_Y_EXCL).contains(&y) {
            return None;
        }
        let ly = (y - MIN_Y) as usize;
        let chunk = (z / 16) * CHUNKS_PER_SIDE + x / 16;
        Some((chunk, ly / 16, Section::index(x % 16, ly % 16, z % 16)))
    }

    pub fn chunk(&self, cx: usize, cz: usize) -> &Chunk {
        &self.chunks[cz * CHUNKS_PER_SIDE + cx]
    }

    pub fn intern(&mut self, s: &BlockState) -> BlockId {
        self.palette.intern(s)
    }

    /// Block at local `(x, y, z)`; air outside the tile.
    pub fn get(&self, x: usize, y: i32, z: usize) -> BlockId {
        match Self::locate(x, y, z) {
            Some((c, s, i)) => self.chunks[c].sections[s].as_ref().map_or(AIR, |sec| sec.blocks[i]),
            None => AIR,
        }
    }

    /// Sets local `(x, y, z)`; returns false (and writes nothing) outside the tile.
    pub fn set(&mut self, x: usize, y: i32, z: usize, id: BlockId) -> bool {
        let Some((c, s, i)) = Self::locate(x, y, z) else { return false };
        let slot = &mut self.chunks[c].sections[s];
        match slot {
            Some(sec) => sec.blocks[i] = id,
            None if id == AIR => {}
            None => {
                let mut sec = Section::filled(AIR);
                sec.blocks[i] = id;
                *slot = Some(sec);
            }
        }
        true
    }

    /// Fills `y_from..=y_to` of column `(x, z)`.
    pub fn fill_column(&mut self, x: usize, z: usize, y_from: i32, y_to: i32, id: BlockId) {
        for y in y_from.max(MIN_Y)..=y_to.min(MAX_Y_EXCL - 1) {
            self.set(x, y, z, id);
        }
    }

    pub fn biome_index(&mut self, name: &str) -> u8 {
        if let Some(i) = self.biomes.iter().position(|b| b == name) {
            return i as u8;
        }
        self.biomes.push(name.to_owned());
        u8::try_from(self.biomes.len() - 1).expect("more than 256 biomes")
    }

    /// Sets the biome of the 4 × 4 × 4 cell containing local `(x, y, z)`.
    pub fn set_biome(&mut self, x: usize, y: i32, z: usize, biome: u8) {
        let Some((c, s, _)) = Self::locate(x, y, z) else { return };
        let ly = ((y - MIN_Y) as usize % 16) / 4;
        let slot = &mut self.chunks[c].biomes[s];
        let cell = (ly * 4 + (z % 16) / 4) * 4 + (x % 16) / 4;
        match slot {
            Some(b) => b[cell] = biome,
            None if biome == 0 => {}
            None => {
                let mut b: SectionBiomes = vec![0u8; 64].into_boxed_slice();
                b[cell] = biome;
                *slot = Some(b);
            }
        }
    }

    /// Replaces sections that ended up all air with `None`.
    pub fn prune(&mut self) {
        for ch in &mut self.chunks {
            for s in &mut ch.sections {
                if s.as_ref().and_then(Section::is_uniform) == Some(AIR) {
                    *s = None;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn block_state_parse_display() {
        let b = BlockState::parse("oak_stairs[half=bottom,facing=north]").unwrap();
        assert_eq!(b.to_string(), "minecraft:oak_stairs[facing=north,half=bottom]");
        assert_eq!(BlockState::parse("minecraft:stone").unwrap(), BlockState::new("stone"));
        assert!(BlockState::parse("stone[facing").is_err());
    }

    #[test]
    fn set_get_bounds() {
        let mut t = VoxelTile::new(TileKey::new(0, 0), "minecraft:plains");
        let stone = t.intern(&BlockState::new("stone"));
        assert!(t.set(0, -64, 0, stone));
        assert!(t.set(511, 319, 511, stone));
        assert!(!t.set(512, 0, 0, stone));
        assert!(!t.set(0, 320, 0, stone));
        assert!(!t.set(0, -65, 0, stone));
        assert_eq!(t.get(0, -64, 0), stone);
        assert_eq!(t.get(511, 319, 511), stone);
        assert_eq!(t.get(1, -64, 0), AIR);
        // Only the two touched sections exist.
        let n: usize = t.chunks.iter().map(|c| c.sections.iter().flatten().count()).sum();
        assert_eq!(n, 2);
        t.set(0, -64, 0, AIR);
        t.prune();
        let n: usize = t.chunks.iter().map(|c| c.sections.iter().flatten().count()).sum();
        assert_eq!(n, 1);
    }

    #[test]
    fn palette_starts_with_air() {
        let mut p = Palette::default();
        assert_eq!(p.intern(&BlockState::air()), AIR);
        assert_eq!(p.intern(&BlockState::new("stone")), 1);
        assert_eq!(p.intern(&BlockState::new("minecraft:stone")), 1);
    }
}
