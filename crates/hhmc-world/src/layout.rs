//! On-disk world layout of the pinned version.
//!
//! Since 26.1 the overworld lives in `dimensions/minecraft/overworld/` (region, entities,
//! poi, data), not in the world root (`DimensionType.getStorageFolder` in the 26.3 jar;
//! wiki *Java Edition 26.1*). World generation settings and game rules moved out of
//! `level.dat` into `data/minecraft/world_gen_settings.dat` and `game_rules.dat`.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;

use hhmc_core::TileKey;

#[derive(Clone, Debug)]
pub struct WorldLayout {
    pub root: PathBuf,
}

impl WorldLayout {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn level_dat(&self) -> PathBuf {
        self.root.join("level.dat")
    }

    pub fn overworld(&self) -> PathBuf {
        self.root.join("dimensions/minecraft/overworld")
    }

    pub fn region_dir(&self) -> PathBuf {
        self.overworld().join("region")
    }

    pub fn region_file(&self, key: TileKey) -> PathBuf {
        self.region_dir().join(key.region_file_name())
    }

    pub fn game_rules_dat(&self) -> PathBuf {
        self.root.join("data/minecraft/game_rules.dat")
    }

    pub fn world_gen_settings_dat(&self) -> PathBuf {
        self.root.join("data/minecraft/world_gen_settings.dat")
    }
}

/// Recursively copies `src` to `dst`, skipping entries for which `keep` is false.
pub fn copy_dir_filtered(src: &Path, dst: &Path, keep: &dyn Fn(&Path) -> bool) -> Result<()> {
    fs::create_dir_all(dst)?;
    for e in fs::read_dir(src)? {
        let e = e?;
        let p = e.path();
        if !keep(&p) {
            continue;
        }
        let target = dst.join(e.file_name());
        if e.file_type()?.is_dir() {
            copy_dir_filtered(&p, &target, keep)?;
        } else {
            fs::copy(&p, &target)?;
        }
    }
    Ok(())
}
