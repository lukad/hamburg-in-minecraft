//! Configuration files under `config/` and the repository's directory layout.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

/// Locations of everything the pipeline reads and writes, relative to the repo root.
#[derive(Clone, Debug)]
pub struct Paths {
    pub root: PathBuf,
}

impl Paths {
    /// Uses `$HHMC_ROOT` if set, otherwise the nearest ancestor of the current
    /// directory that contains `config/world.toml`.
    pub fn discover() -> Result<Self> {
        if let Some(root) = std::env::var_os("HHMC_ROOT") {
            return Ok(Self { root: root.into() });
        }
        let cwd = std::env::current_dir()?;
        for dir in cwd.ancestors() {
            if dir.join("config/world.toml").is_file() {
                return Ok(Self { root: dir.to_path_buf() });
            }
        }
        bail!("no config/world.toml found above {}; set HHMC_ROOT", cwd.display())
    }

    pub fn config(&self) -> PathBuf {
        self.root.join("config")
    }
    pub fn sources_toml(&self) -> PathBuf {
        self.root.join("data/sources.toml")
    }
    pub fn raw(&self) -> PathBuf {
        self.root.join("data/raw")
    }
    pub fn prepared(&self) -> PathBuf {
        self.root.join("data/prepared")
    }
    pub fn cache(&self) -> PathBuf {
        self.root.join("data/cache")
    }
    pub fn out(&self) -> PathBuf {
        self.root.join("out")
    }
}

/// Reads and parses a TOML file into `T`.
pub fn load_toml<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))
}

/// `config/world.toml`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorldConfig {
    pub minecraft: Minecraft,
    pub origin: Origin,
    pub spawn: Spawn,
    pub height: HeightMapping,
    /// Game rules patched into the world, `id → bool | int`, ids without namespace.
    #[serde(default)]
    pub game_rules: toml::Table,
}

impl WorldConfig {
    pub fn load(paths: &Paths) -> Result<Self> {
        load_toml(&paths.config().join("world.toml"))
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Minecraft {
    /// Version id as in Mojang's version manifest.
    pub version: String,
    /// `DataVersion` written into every chunk and `level.dat`.
    pub data_version: i32,
    /// Java major version the dedicated server requires.
    pub java_major: u32,
}

/// World spawn in EPSG:25832; y comes from the generated ground.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct Spawn {
    pub e: f64,
    pub n: f64,
}

/// EPSG:25832 coordinates of world block (0, 0)'s north-west corner.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Origin {
    pub e0: i64,
    pub n0: i64,
}

impl Origin {
    /// World block `(x, z)` containing the EPSG:25832 point `(e, n)`.
    pub fn block_at(&self, e: f64, n: f64) -> (i32, i32) {
        ((e - self.e0 as f64).floor() as i32, (self.n0 as f64 - n).floor() as i32)
    }

    /// EPSG:25832 center of world block `(x, z)`.
    pub fn block_center(&self, x: i32, z: i32) -> (f64, f64) {
        (self.e0 as f64 + x as f64 + 0.5, self.n0 as f64 - z as f64 - 0.5)
    }
}

/// Vertical mapping `y = round(H_NHN)`, clamped to `[clamp_min, clamp_max]`.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct HeightMapping {
    /// Lowest buildable y of the dimension.
    pub min_y: i32,
    /// Highest buildable y of the dimension.
    pub max_y: i32,
    /// Heights are clamped into this range; `min_y` itself is reserved for bedrock.
    pub clamp_min: i32,
    pub clamp_max: i32,
}

/// Result of mapping a height to a block layer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MappedY {
    pub y: i32,
    /// True if the height was outside the clamp range; callers must log this.
    pub clamped: bool,
}

impl HeightMapping {
    pub fn to_y(&self, h_nhn: f64) -> MappedY {
        let y = h_nhn.round();
        if y < self.clamp_min as f64 {
            MappedY { y: self.clamp_min, clamped: true }
        } else if y > self.clamp_max as f64 {
            MappedY { y: self.clamp_max, clamped: true }
        } else {
            MappedY { y: y as i32, clamped: false }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const H: HeightMapping =
        HeightMapping { min_y: -64, max_y: 319, clamp_min: -63, clamp_max: 318 };

    #[test]
    fn height_rounding_and_clamping() {
        assert_eq!(H.to_y(0.49), MappedY { y: 0, clamped: false });
        assert_eq!(H.to_y(0.5), MappedY { y: 1, clamped: false });
        assert_eq!(H.to_y(-17.4), MappedY { y: -17, clamped: false });
        assert_eq!(H.to_y(279.8), MappedY { y: 280, clamped: false });
        assert_eq!(H.to_y(-80.0), MappedY { y: -63, clamped: true });
        assert_eq!(H.to_y(400.0), MappedY { y: 318, clamped: true });
    }

    #[test]
    fn block_mapping() {
        let o = Origin { e0: 565760, n0: 5934080 };
        assert_eq!(o.block_at(565760.2, 5934079.9), (0, 0));
        assert_eq!(o.block_at(565759.9, 5934080.1), (-1, -1));
        assert_eq!(o.block_center(0, 0), (565760.5, 5934079.5));
        let (e, n) = o.block_center(-300, 1234);
        assert_eq!(o.block_at(e, n), (-300, 1234));
    }
}
