//! Anvil region container (`r.<rx>.<rz>.mca`).
//!
//! Layout (wiki *Region file format*; `RegionFile` in the 26.3 jar): 4096-byte sectors;
//! sector 0 = 1024 big-endian u32 locations (`offset << 8 | sector_count`), sector 1 =
//! 1024 big-endian u32 timestamps (epoch seconds). Entry index = `(x & 31) + (z & 31) * 32`.
//! Each chunk: big-endian i32 length (payload + 1), compression byte, payload, padded to
//! whole sectors. Compression 1 = gzip, 2 = zlib, 3 = none, 4 = LZ4, 127 = custom;
//! +128 = payload stored in an external `c.<x>.<z>.mcc` file (≥ 256 sectors).

use std::io::{Read, Write};

use anyhow::{Result, bail, ensure};
use flate2::Compression;
use flate2::read::{GzDecoder, ZlibDecoder};
use flate2::write::ZlibEncoder;

pub const SECTOR: usize = 4096;
pub const COMPRESSION_ZLIB: u8 = 2;
const MAX_SECTORS: usize = 255;

/// zlib-compresses one chunk's NBT into a region chunk record payload.
pub fn compress(nbt: &[u8]) -> Vec<u8> {
    let mut enc = ZlibEncoder::new(Vec::with_capacity(nbt.len() / 4), Compression::new(6));
    enc.write_all(nbt).expect("in-memory write");
    enc.finish().expect("in-memory write")
}

/// Assembles a region file from up to 1024 zlib-compressed chunks, indexed
/// `local_x + local_z * 32`. Timestamps are written as 0 so output is deterministic.
pub fn assemble(chunks: &[Option<Vec<u8>>]) -> Result<Vec<u8>> {
    ensure!(chunks.len() == 1024, "a region has 1024 chunk slots");
    let mut out = vec![0u8; 2 * SECTOR];
    for (i, c) in chunks.iter().enumerate() {
        let Some(data) = c else { continue };
        let record = data.len() + 5;
        let sectors = record.div_ceil(SECTOR);
        if sectors > MAX_SECTORS {
            bail!("chunk {i} needs {sectors} sectors; external .mcc chunks are not supported");
        }
        let offset = out.len() / SECTOR;
        let loc = ((offset as u32) << 8) | sectors as u32;
        out[i * 4..i * 4 + 4].copy_from_slice(&loc.to_be_bytes());
        out.extend_from_slice(&((data.len() + 1) as u32).to_be_bytes());
        out.push(COMPRESSION_ZLIB);
        out.extend_from_slice(data);
        out.resize((offset + sectors) * SECTOR, 0);
    }
    Ok(out)
}

/// Splits a region file into decompressed chunk NBT, indexed `local_x + local_z * 32`.
pub fn disassemble(region: &[u8]) -> Result<Vec<Option<Vec<u8>>>> {
    ensure!(region.len() >= 2 * SECTOR, "region file shorter than its header");
    let mut out = Vec::with_capacity(1024);
    for i in 0..1024 {
        let loc = u32::from_be_bytes(region[i * 4..i * 4 + 4].try_into()?);
        if loc == 0 {
            out.push(None);
            continue;
        }
        let start = (loc >> 8) as usize * SECTOR;
        ensure!(start + 5 <= region.len(), "chunk {i} offset beyond end of file");
        let len = u32::from_be_bytes(region[start..start + 4].try_into()?) as usize;
        let kind = region[start + 4];
        ensure!(len >= 1 && start + 4 + len <= region.len(), "chunk {i} length out of range");
        let payload = &region[start + 5..start + 4 + len];
        let mut nbt = Vec::new();
        match kind {
            1 => {
                GzDecoder::new(payload).read_to_end(&mut nbt)?;
            }
            2 => {
                ZlibDecoder::new(payload).read_to_end(&mut nbt)?;
            }
            3 => nbt.extend_from_slice(payload),
            k => bail!("chunk {i}: unsupported compression type {k}"),
        }
        out.push(Some(nbt));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assemble_disassemble() {
        let mut chunks = vec![None; 1024];
        chunks[0] = Some(compress(b"hello"));
        chunks[1023] = Some(compress(&vec![7u8; 10_000]));
        let region = assemble(&chunks).unwrap();
        assert_eq!(region.len() % SECTOR, 0);
        // First chunk starts at sector 2 and takes one sector.
        assert_eq!(&region[0..4], &[0, 0, 2, 1]);
        let back = disassemble(&region).unwrap();
        assert_eq!(back[0].as_deref(), Some(&b"hello"[..]));
        assert_eq!(back[1023].as_deref(), Some(&vec![7u8; 10_000][..]));
        assert!(back[1..1023].iter().all(Option::is_none));
    }
}
