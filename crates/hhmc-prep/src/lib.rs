//! `hhmc prep`: the global step between fetch and the per-tile stages.
//!
//! - Rasters (DGM1, bDOM, DGM-W) → `data/prepared/<id>/<rx>_<rz>.tif`: EPSG:25832, 1 m,
//!   aligned to the tile grid, covering the tile plus its read buffer, Float32 COG with
//!   NoData −9999 (gdalbuildvrt + gdalwarp).
//! - ALKIS land use → `data/prepared/alkis_nutzung.fgb` (ogr2ogr).
//! - Land-use classes and water bodies → `data/prepared/landuse/<rx>_<rz>.tif` (class
//!   index from config/blocks.toml) and `data/prepared/water/<rx>_<rz>.tif` (body index,
//!   0 = dry) plus `data/prepared/water/bodies.json`.
//! - CityGML buildings (LoD2 and LoD3, chosen per building) → triangle meshes binned per
//!   tile, `data/prepared/buildings/<rx>_<rz>.bin` (zstd postcard `Vec<BuildingMesh>` of
//!   every building whose bounding box touches the tile), `index.json` (id → source,
//!   tiles) and `match.json` (LoD2/LoD3 selection counts).
//!
//! - Trees (street cadasters with 3D-cadaster heights, park trees from the canopy height
//!   model) → `data/prepared/trees/<rx>_<rz>.json`.
//!
//! - ALKIS NAS structures (decks, piers, tracks) → `data/prepared/infra/<rx>_<rz>.json`.
//!
//! Every output carries a `<file>.key` sidecar with the hash of everything it was made
//! from; outputs whose key is unchanged are not rebuilt.

mod barriers;
mod buildings;
mod gdal;
mod infra;
mod landuse;
pub mod nas;
mod rasters;
mod trees;

use anyhow::{Context, Result};
use hhmc_core::{Areas, Paths, TileKey, WorldConfig};
use sha2::{Digest, Sha256};

/// Bump when prep logic changes; part of every prep key.
pub const PREP_VERSION: u32 = 5;

/// Raster datasets normalized per tile: (source id, file glob suffix).
pub const RASTERS: [&str; 3] = ["dgm1", "bdom1", "dgmw"];

pub struct Ctx {
    pub paths: Paths,
    pub world: WorldConfig,
    pub tiles: Vec<TileKey>,
    pub sources: hhmc_fetch::Sources,
}

pub fn run(paths: &Paths, area: &str) -> Result<()> {
    let world = WorldConfig::load(paths)?;
    let tiles = Areas::load(paths)?.get(area)?.tiles.clone();
    let sources = hhmc_fetch::Sources::load(&paths.sources_toml())?;
    let ctx = Ctx { paths: paths.clone(), world, tiles, sources };
    for id in RASTERS {
        rasters::prepare(&ctx, id).with_context(|| format!("preparing {id}"))?;
    }
    rasters::prepare_dop(&ctx)?;
    landuse::vectors(&ctx)?;
    landuse::landuse_and_water(&ctx)?;
    buildings::prepare(&ctx)?;
    // Infra before trees: bridge decks and canopies are not tree crowns.
    infra::prepare(&ctx)?;
    trees::prepare(&ctx)?;
    Ok(())
}

/// Hex sha256 over the given parts, each length-prefixed.
pub fn key_of(parts: &[&[u8]]) -> String {
    let mut h = Sha256::new();
    for p in parts {
        h.update((p.len() as u64).to_le_bytes());
        h.update(p);
    }
    hex::encode(h.finalize())
}

pub(crate) fn key_path(out: &std::path::Path) -> std::path::PathBuf {
    let mut s = out.as_os_str().to_owned();
    s.push(".key");
    s.into()
}

pub(crate) fn up_to_date(out: &std::path::Path, key: &str) -> bool {
    out.exists() && std::fs::read_to_string(key_path(out)).is_ok_and(|k| k.trim() == key)
}

/// Whether an area-wide output is current for `tiles`: its sidecar holds the key and
/// then the tiles it was made for, and a run for a subset of those tiles with the same
/// inputs reuses it. A prep of a smaller area after a larger one thus keeps the larger area's
/// water bodies, buildings, trees and structures instead of overwriting them.
pub(crate) fn covers(out: &std::path::Path, key: &str, tiles: &[TileKey]) -> bool {
    let Ok(s) = std::fs::read_to_string(key_path(out)) else { return false };
    let mut lines = s.lines();
    if !out.exists() || lines.next().map(str::trim) != Some(key) {
        return false;
    }
    let have: std::collections::HashSet<&str> = lines.map(str::trim).collect();
    tiles.iter().all(|t| have.contains(t.to_string().as_str()))
}

/// Records an area-wide output's key and the tiles it was made for (see [`covers`]).
pub(crate) fn mark_covering(out: &std::path::Path, key: &str, tiles: &[TileKey]) -> Result<()> {
    let mut s = format!("{key}\n");
    for t in tiles {
        s.push_str(&format!("{t}\n"));
    }
    std::fs::write(key_path(out), s)?;
    Ok(())
}

pub(crate) fn mark(out: &std::path::Path, key: &str) -> Result<()> {
    std::fs::write(key_path(out), format!("{key}\n"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn area_outputs_serve_subsets_of_their_tiles() {
        let dir = std::env::temp_dir().join(format!("hhmc-prep-covers-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("summary.json");
        std::fs::write(&out, "{}").unwrap();
        let t = |rx, rz| TileKey::new(rx, rz);
        let full = [t(0, 0), t(1, 0), t(0, 1), t(-1, 2)];
        assert!(!covers(&out, "k", &full));
        mark_covering(&out, "k", &full).unwrap();
        assert!(covers(&out, "k", &full));
        assert!(covers(&out, "k", &[t(-1, 2), t(0, 0)]));
        assert!(!covers(&out, "k", &[t(0, 0), t(5, 5)]), "a tile outside");
        assert!(!covers(&out, "other", &[t(0, 0)]), "other inputs");
    }
}
