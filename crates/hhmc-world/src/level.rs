//! Patching the template world's `level.dat` and `game_rules.dat` (gzip NBT).
//!
//! The template is created once by the vanilla dedicated server (void flat generator), so
//! world-generation settings are whatever the pinned version writes itself. Only spawn and
//! game rules are patched. 26.3 keys (from `PrimaryLevelData.setTagData` and the saved-data
//! wrapper): `level.dat` → `Data.spawn{dimension, pos:[I;x,y,z], yaw:F, pitch:F}`;
//! `data/minecraft/game_rules.dat` → `{DataVersion, data:{"minecraft:<rule>": value}}`.

use std::io::{Read, Write};
use std::path::Path;

use anyhow::{Context, Result};
use fastnbt::{IntArray, Value};
use flate2::Compression;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;

pub fn read_gz_nbt(path: &Path) -> Result<Value> {
    let mut raw = Vec::new();
    GzDecoder::new(
        std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?,
    )
    .read_to_end(&mut raw)?;
    fastnbt::from_bytes(&raw).with_context(|| format!("parsing {}", path.display()))
}

pub fn write_gz_nbt(path: &Path, v: &Value) -> Result<()> {
    let bytes = fastnbt::to_bytes(v)?;
    let mut enc = GzEncoder::new(Vec::new(), Compression::default());
    enc.write_all(&bytes)?;
    let tmp = path.with_extension("dat.tmp");
    std::fs::write(&tmp, enc.finish()?)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

fn compound<'a>(
    v: &'a mut Value,
    key: &str,
) -> Result<&'a mut std::collections::HashMap<String, Value>> {
    match v {
        Value::Compound(m) => match m.get_mut(key) {
            Some(Value::Compound(c)) => Ok(c),
            _ => anyhow::bail!("missing compound `{key}`"),
        },
        _ => anyhow::bail!("not a compound"),
    }
}

/// Sets the world spawn in `level.dat` and the level name.
pub fn patch_level_dat(path: &Path, spawn: [i32; 3], level_name: &str) -> Result<()> {
    let mut root = read_gz_nbt(path)?;
    let data = compound(&mut root, "Data")?;
    let mut sp = std::collections::HashMap::new();
    sp.insert("dimension".to_owned(), Value::String("minecraft:overworld".to_owned()));
    sp.insert("pos".to_owned(), Value::IntArray(IntArray::new(spawn.to_vec())));
    sp.insert("yaw".to_owned(), Value::Float(0.0));
    sp.insert("pitch".to_owned(), Value::Float(0.0));
    data.insert("spawn".to_owned(), Value::Compound(sp));
    data.insert("LevelName".to_owned(), Value::String(level_name.to_owned()));
    write_gz_nbt(path, &root)
}

/// A game rule value: booleans are NBT bytes, integers NBT ints.
#[derive(Clone, Debug)]
pub enum RuleValue {
    Bool(bool),
    Int(i32),
}

/// Sets `minecraft:<rule>` entries in `game_rules.dat`.
pub fn patch_game_rules(path: &Path, rules: &[(String, RuleValue)]) -> Result<()> {
    let mut root = read_gz_nbt(path)?;
    let data = compound(&mut root, "data")?;
    for (k, v) in rules {
        let key = if k.contains(':') { k.clone() } else { format!("minecraft:{k}") };
        let val = match v {
            RuleValue::Bool(b) => Value::Byte(i8::from(*b)),
            RuleValue::Int(i) => Value::Int(*i),
        };
        data.insert(key, val);
    }
    write_gz_nbt(path, &root)
}

/// Creates `world` from the template world: copies it (replacing any previous world),
/// then patches spawn, level name and game rules. Region files are written separately.
pub fn assemble_world(
    template: &Path,
    world: &crate::WorldLayout,
    spawn: [i32; 3],
    level_name: &str,
    rules: &[(String, RuleValue)],
) -> Result<()> {
    if world.root.exists() {
        std::fs::remove_dir_all(&world.root)?;
    }
    crate::layout::copy_dir_filtered(template, &world.root, &|_| true)?;
    patch_level_dat(&world.level_dat(), spawn, level_name)?;
    if !rules.is_empty() {
        patch_game_rules(&world.game_rules_dat(), rules)?;
    }
    std::fs::create_dir_all(world.region_dir())?;
    Ok(())
}
