//! Schema of `data/sources.toml`.
//!
//! One `[[source]]` per dataset with its provenance and license, and one
//! `[[source.files]]` per downloadable file (download URL, with a
//! `sha256` and `size_bytes` per file). Sources without files (web services,
//! values copied into `config/`) are listed for attribution only.
//!
//! Large file lists generated for an area (`hhmc sources expand`) live next to it in
//! `data/sources/<id>.toml` (`[[files]]`, same schema) and are appended to the source's
//! `files` on load; pins are written back to the file each entry came from.

use std::path::Path;

use anyhow::{Result, bail};
use hhmc_core::Bbox;
use hhmc_core::config::load_toml;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Sources {
    #[serde(rename = "source", default)]
    pub sources: Vec<Source>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Source {
    pub id: String,
    pub title: String,
    pub publisher: String,
    pub landing_url: String,
    /// Short license id, e.g. `dl-de/by-2-0`, `CC-BY-4.0`, `ODbL-1.0`.
    pub license: String,
    pub license_url: String,
    /// Exact attribution text required by the publisher.
    pub attribution: String,
    /// Date the files were pinned (`YYYY-MM-DD`).
    #[serde(default)]
    pub retrieved: Option<String>,
    pub crs: String,
    /// Files are web-service responses (WFS) that change when the publisher updates the
    /// data. A checksum mismatch then means "re-pin", not "corrupt download".
    #[serde(default)]
    pub volatile: bool,
    #[serde(default)]
    pub vertical_datum: Option<String>,
    /// What the pipeline uses this source for, and which stages read it.
    pub used_for: String,
    #[serde(default)]
    pub notes: Option<String>,
    /// Sum of `files[].size_bytes`, maintained by `--pin`.
    #[serde(default)]
    pub size_bytes: Option<u64>,
    #[serde(default)]
    pub files: Vec<SourceFile>,
    /// How `hhmc sources expand` lists this source's files for an area.
    #[serde(default)]
    pub tiled: Option<Tiled>,
    /// Number of `files` from sources.toml itself; the rest came from
    /// `data/sources/<id>.toml`.
    #[serde(skip)]
    pub main_files: usize,
}

/// Rules for generating a source's file list for an area.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Tiled {
    /// `utm32_km`: zip members named `…_32_<E km>_<N km>_1_…` (1 km tiles);
    /// `lod3_folder`: zip folders `<EE><NN>/` (1 km tile E 5EE km, N 59NN km), extracted
    /// with `member_prefix` into `<path_prefix>/<EENN>`;
    /// `all`: every member of the archives, without a bbox;
    /// `wfs_blocks`: `url_template` with `{minx},{miny},{maxx},{maxy}` per block of
    /// `block_m` over the area.
    pub kind: String,
    #[serde(default)]
    pub archives: Vec<TiledArchive>,
    #[serde(default)]
    pub url_template: Option<String>,
    #[serde(default)]
    pub block_m: Option<f64>,
    /// Only archive members whose name ends with this (e.g. `.tif`).
    #[serde(default)]
    pub suffix: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TiledArchive {
    pub url: String,
    /// Destination folder for this archive's members (`lod3_folder`; with a trailing
    /// `/` for the other kinds), or "".
    #[serde(default)]
    pub path_prefix: String,
}

/// `data/sources/<id>.toml`.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExtraFiles {
    #[serde(default)]
    pub files: Vec<SourceFile>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceFile {
    pub url: String,
    /// If set, `url` is a zip archive and only this member is extracted (read with
    /// HTTP range requests, so the rest of the archive is never downloaded).
    #[serde(default)]
    pub member: Option<String>,
    /// If set, `url` is a zip archive and every member under this folder prefix is
    /// extracted into the directory `path` (e.g. one LoD3 tile with its textures).
    /// `sha256` is then the digest of the sorted `"<sha256>  <relative path>\n"` lines.
    #[serde(default)]
    pub member_prefix: Option<String>,
    /// Destination relative to `data/raw/<source id>/`.
    pub path: String,
    /// sha256 of the stored file (of the extracted member for `member` entries).
    #[serde(default)]
    pub sha256: Option<String>,
    #[serde(default)]
    pub size_bytes: Option<u64>,
    /// Footprint in EPSG:25832. Files without a bbox are needed by every area.
    #[serde(default)]
    pub bbox: Option<Bbox>,
}

impl Sources {
    pub fn load(path: &Path) -> Result<Self> {
        let mut s: Sources = load_toml(path)?;
        for src in &mut s.sources {
            src.main_files = src.files.len();
            let extra = Self::extra_path(path, &src.id);
            if extra.exists() {
                let e: ExtraFiles = load_toml(&extra)?;
                src.files.extend(e.files);
            }
        }
        s.validate()?;
        Ok(s)
    }

    /// `data/sources/<id>.toml` next to `sources.toml`.
    pub fn extra_path(sources_toml: &Path, id: &str) -> std::path::PathBuf {
        sources_toml.parent().unwrap_or(Path::new(".")).join("sources").join(format!("{id}.toml"))
    }

    fn validate(&self) -> Result<()> {
        let mut ids = std::collections::HashSet::new();
        for s in &self.sources {
            if !ids.insert(&s.id) {
                bail!("duplicate source id `{}`", s.id);
            }
            let mut paths = std::collections::HashSet::new();
            for f in &s.files {
                if !paths.insert(&f.path) {
                    bail!("source `{}`: duplicate file path `{}`", s.id, f.path);
                }
            }
            for f in &s.files {
                if f.member.is_some() && f.member_prefix.is_some() {
                    bail!("source `{}`: `{}` has both member and member_prefix", s.id, f.path);
                }
                let p = Path::new(&f.path);
                if p.is_absolute()
                    || p.components().any(|c| matches!(c, std::path::Component::ParentDir))
                {
                    bail!(
                        "source `{}`: file path `{}` must be relative without `..`",
                        s.id,
                        f.path
                    );
                }
            }
        }
        Ok(())
    }

    pub fn get(&self, id: &str) -> Option<&Source> {
        self.sources.iter().find(|s| s.id == id)
    }
}

impl SourceFile {
    /// Whether this file is needed for an area whose read box is `area`.
    pub fn needed_for(&self, area: Option<&Bbox>) -> bool {
        match (area, &self.bbox) {
            (Some(a), Some(b)) => a.intersects(b),
            _ => true,
        }
    }
}
