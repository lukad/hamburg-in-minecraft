//! `VoxelTile` ↔ region file.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result, bail, ensure};
use fastnbt::LongArray;
use hhmc_core::voxel::{CHUNKS_PER_SIDE, MIN_Y, SECTIONS, Section};
use hhmc_core::{AIR, BlockId, BlockState, TileKey, VoxelTile};
use rayon::prelude::*;

use crate::light::SkyLight;
use crate::nbt::{BlockStateNbt, ChunkNbt, Paletted, STATUS_FULL, SectionNbt};
use crate::pack::{biome_bits, block_bits, pack, unpack};
use crate::region;

/// Lowest section index (`Y`) of the overworld.
const MIN_SECTION: i32 = MIN_Y / 16;

fn state_to_nbt(s: &BlockState) -> BlockStateNbt {
    BlockStateNbt::Full { id: s.name.clone(), properties: s.properties.clone() }
}

fn nbt_to_state(n: &BlockStateNbt) -> BlockState {
    match n {
        BlockStateNbt::Full { id, properties } => {
            BlockState { name: id.clone(), properties: properties.clone() }
        }
        BlockStateNbt::Name(id) | BlockStateNbt::Wrapped { name: id } => BlockState::new(id),
    }
}

/// Encodes one section's blocks with a section-local palette in first-seen order.
fn encode_blocks(
    tile: &VoxelTile,
    section: Option<&Section>,
    lut: &mut [u32],
) -> Paletted<BlockStateNbt> {
    let Some(sec) = section else {
        return Paletted { palette: vec![state_to_nbt(tile.palette.get(AIR))], data: None };
    };
    let mut local: Vec<BlockId> = Vec::new();
    let mut idx = Vec::with_capacity(4096);
    for &b in sec.blocks.iter() {
        let slot = &mut lut[b as usize];
        if *slot == u32::MAX {
            *slot = local.len() as u32;
            local.push(b);
        }
        idx.push(*slot);
    }
    for &b in &local {
        lut[b as usize] = u32::MAX;
    }
    let palette = local.iter().map(|&b| state_to_nbt(tile.palette.get(b))).collect::<Vec<_>>();
    let bits = block_bits(palette.len());
    let data = (bits > 0).then(|| LongArray::new(pack(&idx, bits)));
    Paletted { palette, data }
}

fn encode_biomes(tile: &VoxelTile, cells: Option<&[u8]>) -> Paletted<String> {
    let Some(cells) = cells else {
        return Paletted { palette: vec![tile.biomes[0].clone()], data: None };
    };
    let mut local: Vec<u8> = Vec::new();
    let idx: Vec<u32> = cells
        .iter()
        .map(|&b| match local.iter().position(|&l| l == b) {
            Some(p) => p as u32,
            None => {
                local.push(b);
                (local.len() - 1) as u32
            }
        })
        .collect();
    let palette: Vec<String> = local.iter().map(|&b| tile.biomes[b as usize].clone()).collect();
    let bits = biome_bits(palette.len());
    let data = (bits > 0).then(|| LongArray::new(pack(&idx, bits)));
    Paletted { palette, data }
}

/// Serializes chunk `(cx, cz)` of `tile` to uncompressed NBT. Every section is written,
/// so chunks never fall back to the generator or the default biome.
pub fn chunk_nbt(
    tile: &VoxelTile,
    cx: usize,
    cz: usize,
    data_version: i32,
    light: Option<&SkyLight>,
) -> Result<Vec<u8>> {
    let chunk = tile.chunk(cx, cz);
    let mut lut = vec![u32::MAX; tile.palette.len()];
    let sections = (0..SECTIONS)
        .map(|s| SectionNbt {
            y: (MIN_SECTION + s as i32) as i8,
            block_states: Some(encode_blocks(tile, chunk.sections[s].as_ref(), &mut lut)),
            biomes: Some(encode_biomes(tile, chunk.biomes[s].as_deref())),
            sky_light: light.and_then(|l| l.section(cx, cz, s)).map(fastnbt::ByteArray::new),
        })
        .collect();
    let nbt = ChunkNbt {
        data_version,
        x_pos: tile.key.rx * CHUNKS_PER_SIDE as i32 + cx as i32,
        y_pos: MIN_SECTION,
        z_pos: tile.key.rz * CHUNKS_PER_SIDE as i32 + cz as i32,
        status: STATUS_FULL.to_owned(),
        last_update: 0,
        inhabited_time: 0,
        sections,
    };
    Ok(fastnbt::to_bytes(&nbt)?)
}

/// Encodes a whole tile as region file bytes (chunks compressed in parallel).
pub fn tile_to_region(tile: &VoxelTile, data_version: i32) -> Result<Vec<u8>> {
    let light = SkyLight::of(tile);
    let chunks: Vec<Option<Vec<u8>>> = (0..CHUNKS_PER_SIDE * CHUNKS_PER_SIDE)
        .into_par_iter()
        .map(|i| {
            let (cx, cz) = (i % CHUNKS_PER_SIDE, i / CHUNKS_PER_SIDE);
            chunk_nbt(tile, cx, cz, data_version, Some(&light)).map(|n| Some(region::compress(&n)))
        })
        .collect::<Result<_>>()?;
    region::assemble(&chunks)
}

/// Writes `tile` to `dir/r.<rx>.<rz>.mca` atomically.
pub fn write_tile(tile: &VoxelTile, dir: &Path, data_version: i32) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    let bytes = tile_to_region(tile, data_version)?;
    let path = dir.join(tile.key.region_file_name());
    let tmp = path.with_extension("mca.tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

/// Reads a region file back into a `VoxelTile` (biomes ignored). Missing chunks are air.
pub fn read_tile(path: &Path, key: TileKey) -> Result<VoxelTile> {
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let chunks = region::disassemble(&bytes)?;
    let mut tile = VoxelTile::new(key, "minecraft:plains");
    for (i, c) in chunks.iter().enumerate() {
        let Some(nbt) = c else { continue };
        let (cx, cz) = (i % 32, i / 32);
        let chunk: ChunkNbt =
            fastnbt::from_bytes(nbt).with_context(|| format!("chunk {cx},{cz}"))?;
        let want = (key.rx * 32 + cx as i32, key.rz * 32 + cz as i32);
        ensure!(
            (chunk.x_pos, chunk.z_pos) == want,
            "chunk {cx},{cz} has position {},{}",
            chunk.x_pos,
            chunk.z_pos
        );
        for sec in &chunk.sections {
            let s = sec.y as i32 - MIN_SECTION;
            if !(0..SECTIONS as i32).contains(&s) {
                continue;
            }
            let Some(bs) = &sec.block_states else { continue };
            let ids: Vec<BlockId> =
                bs.palette.iter().map(|p| tile.intern(&nbt_to_state(p))).collect();
            let idx: Vec<u32> = match (&bs.data, ids.len()) {
                (None, 1) => vec![0; 4096],
                (Some(d), n) => {
                    let bits = block_bits(n).max(crate::pack::ceil_log2(n));
                    match unpack(d, bits, 4096) {
                        Some(v) => v,
                        None => bail!("chunk {cx},{cz} section {}: bad data length", sec.y),
                    }
                }
                (None, n) => {
                    bail!("chunk {cx},{cz} section {}: {n} palette entries but no data", sec.y)
                }
            };
            let mut cache: HashMap<u32, BlockId> = HashMap::new();
            for (j, &v) in idx.iter().enumerate() {
                let id =
                    *cache.entry(v).or_insert_with(|| ids.get(v as usize).copied().unwrap_or(AIR));
                if id == AIR {
                    continue;
                }
                let (x, z, y) = (j % 16, (j / 16) % 16, j / 256);
                tile.set(cx * 16 + x, (s * 16 + y as i32) + MIN_Y, cz * 16 + z, id);
            }
        }
    }
    Ok(tile)
}

/// First difference between two tiles, comparing block states (not palette ids).
pub fn first_difference(
    a: &VoxelTile,
    b: &VoxelTile,
) -> Option<(usize, i32, usize, String, String)> {
    for z in 0..512 {
        for x in 0..512 {
            for y in MIN_Y..MIN_Y + 16 * SECTIONS as i32 {
                let (sa, sb) = (a.palette.get(a.get(x, y, z)), b.palette.get(b.get(x, y, z)));
                if sa != sb {
                    return Some((x, y, z, sa.to_string(), sb.to_string()));
                }
            }
        }
    }
    None
}
