//! Per-(stage, tile) result cache: `data/cache/<stage>/<rx>_<rz>.bin` holds a magic, the
//! 32-byte cache key and the zstd-compressed postcard encoding of the result. A stage's
//! key covers its version constant, its config, the content of its inputs and its
//! upstream keys (see [`KeyBuilder`]).

use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::Serialize;
use serde::de::DeserializeOwned;
use sha2::{Digest, Sha256};

use crate::TileKey;

const MAGIC: &[u8; 8] = b"HHMCC001";

/// Accumulates everything a stage result depends on.
#[derive(Clone, Default)]
pub struct KeyBuilder(Sha256);

impl KeyBuilder {
    pub fn new(stage: &str, version: u32) -> Self {
        let mut k = Self(Sha256::new());
        k.bytes(stage.as_bytes()).bytes(&version.to_le_bytes());
        k
    }

    /// Adds a length-prefixed byte string.
    pub fn bytes(&mut self, b: &[u8]) -> &mut Self {
        self.0.update((b.len() as u64).to_le_bytes());
        self.0.update(b);
        self
    }

    /// Adds a file's content (via its sha256), or a marker if it does not exist.
    pub fn file(&mut self, p: &Path) -> Result<&mut Self> {
        match std::fs::read(p) {
            Ok(b) => {
                let d = Sha256::digest(&b);
                self.bytes(&d);
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                self.bytes(b"<missing>");
            }
            Err(e) => return Err(e.into()),
        }
        Ok(self)
    }

    pub fn finish(&self) -> [u8; 32] {
        self.0.clone().finalize().into()
    }
}

#[derive(Clone, Debug)]
pub struct Cache {
    pub root: PathBuf,
}

impl Cache {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn path(&self, stage: &str, tile: TileKey) -> PathBuf {
        self.root.join(stage).join(format!("{}_{}.bin", tile.rx, tile.rz))
    }

    /// The cached result if its key matches.
    pub fn get<T: DeserializeOwned>(
        &self,
        stage: &str,
        tile: TileKey,
        key: &[u8; 32],
    ) -> Option<T> {
        let b = std::fs::read(self.path(stage, tile)).ok()?;
        if b.len() < 40 || &b[..8] != MAGIC || &b[8..40] != key {
            return None;
        }
        let raw = zstd::decode_all(&b[40..]).ok()?;
        postcard::from_bytes(&raw).ok()
    }

    pub fn put<T: Serialize>(
        &self,
        stage: &str,
        tile: TileKey,
        key: &[u8; 32],
        v: &T,
    ) -> Result<()> {
        let path = self.path(stage, tile);
        std::fs::create_dir_all(path.parent().expect("has parent"))?;
        let raw = postcard::to_stdvec(v)?;
        let mut out = Vec::with_capacity(raw.len() / 4 + 40);
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(key);
        out.extend_from_slice(&zstd::encode_all(&raw[..], 3)?);
        let tmp = path.with_extension("bin.tmp");
        std::fs::write(&tmp, out)?;
        std::fs::rename(tmp, path)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hit_and_miss() {
        let dir = std::env::temp_dir().join(format!("hhmc-cache-{}", std::process::id()));
        let c = Cache::new(&dir);
        let t = TileKey::new(-2, 3);
        let k1 = KeyBuilder::new("s", 1).bytes(b"a").finish();
        let k2 = KeyBuilder::new("s", 2).bytes(b"a").finish();
        assert_ne!(k1, k2);
        c.put("s", t, &k1, &vec![1u16, 2, 3]).unwrap();
        assert_eq!(c.get::<Vec<u16>>("s", t, &k1), Some(vec![1, 2, 3]));
        assert_eq!(c.get::<Vec<u16>>("s", t, &k2), None);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
