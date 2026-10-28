//! Landmark overrides: schematics built by hand, placed over the generated
//! world from `config/landmarks.toml`.
//!
//! Each landmark names a schematic (Sponge Schematic v3 `.schem` or a vanilla structure
//! `.nbt`), the schematic block that goes to an anchor point (EPSG:25832 plus a height in
//! m NHN), a rotation (clockwise seen from above, in quarter turns) and a footprint polygon.
//! In every tile the footprint touches, the generated blocks inside it are cleared above
//! the terrain (and its water) first; then the schematic's blocks are placed, rotated with
//! their block states. Air in the schematic places nothing: the footprint decides what is
//! cleared. Block entities (sign texts, container contents) and entities are not placed.
//!
//! Formats:
//! - Sponge Schematic v3 (github.com/SpongePowered/Schematic-Specification,
//!   versions/schematic-3.md): gzipped NBT, root compound holding `Schematic` with `Width`,
//!   `Height`, `Length` (Short), `Blocks.Palette` (block state string → Int) and
//!   `Blocks.Data` (Byte Array of varint palette indices, index `x + z·W + y·W·L`).
//! - Vanilla structure file (minecraft.wiki/w/Structure_file): `size` (3 Int), `palette`
//!   (Compounds with `Name` and optional `Properties`), `blocks` (Compounds with `state`
//!   and `pos`); the first palette of `palettes` when `palette` is absent.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use fastnbt::Value;
use hhmc_core::config::{HeightMapping, Origin};
use hhmc_core::geom::rasterize_polygon;
use hhmc_core::ground::{GroundModel, NO_WATER};
use hhmc_core::voxel::{BlockState, MAX_Y_EXCL};
use hhmc_core::{TILE_SIZE, TileKey};
use serde::{Deserialize, Serialize};

/// Bump when the landmark logic changes.
pub const LANDMARKS_VERSION: u32 = 1;

/// `config/landmarks.toml`.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LandmarksConfig {
    #[serde(default)]
    pub landmark: Vec<Landmark>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Landmark {
    pub name: String,
    /// Schematic file, relative to the repository root.
    pub schematic: PathBuf,
    /// EPSG:25832 easting, northing and height (m NHN) of the anchor.
    pub anchor: [f64; 3],
    /// Schematic block (x, y, z, in the schematic's own coordinates) placed at the anchor;
    /// the rotation turns around it.
    #[serde(default)]
    pub origin: [i32; 3],
    /// Clockwise quarter turns seen from above: 0, 90, 180 or 270.
    #[serde(default)]
    pub rotation: u32,
    /// Footprint ring, EPSG:25832 (E, N) points: generated blocks inside it are cleared
    /// above the terrain and its water before the schematic is placed.
    pub footprint: Vec<[f64; 2]>,
}

impl LandmarksConfig {
    pub fn path(root: &Path) -> PathBuf {
        root.join("config/landmarks.toml")
    }

    pub fn load(root: &Path) -> Result<Self> {
        let p = Self::path(root);
        if !p.exists() {
            return Ok(Self::default());
        }
        let c: Self = hhmc_core::config::load_toml(&p)?;
        for l in &c.landmark {
            ensure!(
                [0, 90, 180, 270].contains(&l.rotation),
                "landmark {}: rotation must be 0, 90, 180 or 270",
                l.name
            );
            ensure!(l.footprint.len() >= 3, "landmark {}: footprint needs 3 points", l.name);
        }
        Ok(c)
    }
}

/// A schematic's non-air blocks in its own coordinates.
#[derive(Clone, Debug, PartialEq)]
pub struct Schematic {
    pub data_version: i32,
    pub size: [i32; 3],
    pub palette: Vec<BlockState>,
    /// (x, y, z, palette index).
    pub blocks: Vec<([i32; 3], u32)>,
}

/// Blocks that place nothing.
fn is_empty(s: &BlockState) -> bool {
    matches!(s.name.as_str(), "minecraft:air" | "minecraft:structure_void")
}

/// Reads a Sponge v3 `.schem` or a vanilla structure `.nbt` (gzipped or not).
pub fn load_schematic(path: &Path) -> Result<Schematic> {
    let raw = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let bytes = if raw.starts_with(&[0x1f, 0x8b]) {
        let mut out = Vec::new();
        flate2::read::GzDecoder::new(&raw[..]).read_to_end(&mut out)?;
        out
    } else {
        raw
    };
    let root: Value =
        fastnbt::from_bytes(&bytes).with_context(|| format!("NBT of {}", path.display()))?;
    let Value::Compound(root) = root else { bail!("{}: root is not a compound", path.display()) };
    match root.get("Schematic") {
        Some(Value::Compound(s)) => sponge_v3(s),
        _ if root.contains_key("palette") || root.contains_key("palettes") => structure(&root),
        _ => bail!("{}: neither a Sponge v3 schematic nor a structure file", path.display()),
    }
    .with_context(|| format!("schematic {}", path.display()))
}

fn int(v: Option<&Value>, what: &str) -> Result<i32> {
    match v {
        Some(Value::Int(i)) => Ok(*i),
        Some(Value::Short(i)) => Ok(i32::from(*i as u16)),
        Some(Value::Byte(i)) => Ok(i32::from(*i)),
        _ => bail!("missing or non-integer {what}"),
    }
}

fn sponge_v3(s: &std::collections::HashMap<String, Value>) -> Result<Schematic> {
    let version = int(s.get("Version"), "Version")?;
    ensure!(version == 3, "Sponge schematic version {version}, only 3 is supported");
    let data_version = int(s.get("DataVersion"), "DataVersion")?;
    let (w, h, l) = (
        int(s.get("Width"), "Width")?,
        int(s.get("Height"), "Height")?,
        int(s.get("Length"), "Length")?,
    );
    let Some(Value::Compound(blocks)) = s.get("Blocks") else { bail!("no Blocks") };
    let Some(Value::Compound(pal)) = blocks.get("Palette") else { bail!("no Blocks.Palette") };
    let mut palette = vec![BlockState::new("air"); pal.len()];
    for (k, v) in pal {
        let i = int(Some(v), "palette index")? as usize;
        ensure!(i < palette.len(), "palette index {i} out of range");
        palette[i] = BlockState::parse(k).map_err(anyhow::Error::msg)?;
    }
    let Some(Value::ByteArray(data)) = blocks.get("Data") else { bail!("no Blocks.Data") };
    let mut out = Vec::new();
    let (mut value, mut shift, mut index) = (0u32, 0u32, 0i64);
    for &b in data.iter() {
        let b = b as u8;
        value |= u32::from(b & 0x7f) << shift;
        if b & 0x80 != 0 {
            shift += 7;
            ensure!(shift < 35, "varint too long");
            continue;
        }
        let (x, z, y) = (
            index % i64::from(w),
            (index / i64::from(w)) % i64::from(l),
            index / (i64::from(w) * i64::from(l)),
        );
        ensure!((value as usize) < palette.len(), "block index {value} outside the palette");
        if !is_empty(&palette[value as usize]) {
            out.push(([x as i32, y as i32, z as i32], value));
        }
        (value, shift, index) = (0, 0, index + 1);
    }
    ensure!(
        index == i64::from(w) * i64::from(h) * i64::from(l),
        "Data holds {index} blocks, not {}",
        w * h * l
    );
    Ok(Schematic { data_version, size: [w, h, l], palette, blocks: out })
}

fn structure(root: &std::collections::HashMap<String, Value>) -> Result<Schematic> {
    let data_version = int(root.get("DataVersion"), "DataVersion").unwrap_or(0);
    let ints = |v: Option<&Value>, what: &str| -> Result<[i32; 3]> {
        let Some(Value::List(l)) = v else { bail!("no {what}") };
        ensure!(l.len() == 3, "{what} is not 3 values");
        Ok([int(Some(&l[0]), what)?, int(Some(&l[1]), what)?, int(Some(&l[2]), what)?])
    };
    let size = ints(root.get("size"), "size")?;
    let pal = match (root.get("palette"), root.get("palettes")) {
        (Some(Value::List(p)), _) => p,
        (None, Some(Value::List(ps))) => match ps.first() {
            Some(Value::List(p)) => p,
            _ => bail!("empty palettes"),
        },
        _ => bail!("no palette"),
    };
    let palette = pal
        .iter()
        .map(|e| {
            let Value::Compound(c) = e else { bail!("palette entry is not a compound") };
            let Some(Value::String(name)) = c.get("Name") else {
                bail!("palette entry without Name")
            };
            let mut s = BlockState::new(name);
            if let Some(Value::Compound(props)) = c.get("Properties") {
                for (k, v) in props {
                    let Value::String(v) = v else { bail!("property {k} is not a string") };
                    s = s.with(k, v);
                }
            }
            Ok(s)
        })
        .collect::<Result<Vec<_>>>()?;
    let Some(Value::List(bl)) = root.get("blocks") else { bail!("no blocks") };
    let mut blocks = Vec::new();
    for b in bl {
        let Value::Compound(c) = b else { bail!("block is not a compound") };
        let state = int(c.get("state"), "state")? as u32;
        ensure!((state as usize) < palette.len(), "state {state} outside the palette");
        if !is_empty(&palette[state as usize]) {
            blocks.push((ints(c.get("pos"), "pos")?, state));
        }
    }
    Ok(Schematic { data_version, size, palette, blocks })
}

const DIRS: [&str; 4] = ["north", "east", "south", "west"];

fn turn_dir(d: &str, q: u32) -> Option<&'static str> {
    let i = DIRS.iter().position(|x| *x == d)?;
    Some(DIRS[(i + q as usize) % 4])
}

/// The block state turned `q` quarter turns clockwise (seen from above).
pub fn rotate_state(s: &BlockState, q: u32) -> BlockState {
    let q = q % 4;
    if q == 0 {
        return s.clone();
    }
    let mut out = BlockState { name: s.name.clone(), properties: BTreeMap::new() };
    for (k, v) in &s.properties {
        let (k2, v2) = match k.as_str() {
            // Sides a block connects to (fences, walls, panes, vines, redstone wire, …).
            d if DIRS.contains(&d) => (turn_dir(d, q).expect("dir").to_string(), v.clone()),
            "facing" => (k.clone(), turn_dir(v, q).map_or_else(|| v.clone(), str::to_string)),
            "axis" if q % 2 == 1 => (
                k.clone(),
                match v.as_str() {
                    "x" => "z".into(),
                    "z" => "x".into(),
                    o => o.into(),
                },
            ),
            // Standing signs, banners, heads: 16 steps.
            "rotation" => match v.parse::<u32>() {
                Ok(r) => (k.clone(), ((r + 4 * q) % 16).to_string()),
                Err(_) => (k.clone(), v.clone()),
            },
            // Rail shapes (stairs shapes are relative to their facing and stay).
            "shape" | "orientation" => (k.clone(), turn_words(v, q, k == "shape")),
            _ => (k.clone(), v.clone()),
        };
        out.properties.insert(k2, v2);
    }
    out
}

/// Turns the direction words of a `_`-joined value; rail corners keep their canonical
/// north/south-first order.
fn turn_words(v: &str, q: u32, rail: bool) -> String {
    let parts: Vec<&str> = v.split('_').collect();
    if !parts.iter().any(|p| DIRS.contains(p)) {
        return v.to_string();
    }
    let turned: Vec<&str> = parts.iter().map(|p| turn_dir(p, q).unwrap_or(p)).collect();
    if rail && turned.len() == 2 && turned.iter().all(|p| DIRS.contains(p)) {
        let has = |d: &str| turned.contains(&d);
        // north_south / east_west straights, and corners named north|south first.
        if (has("north") && has("south")) || (has("east") && has("west")) {
            return if has("north") { "north_south".into() } else { "east_west".into() };
        }
        let ns = if has("north") { "north" } else { "south" };
        let ew = if has("east") { "east" } else { "west" };
        return format!("{ns}_{ew}");
    }
    turned.join("_")
}

/// A landmark ready to place: its blocks in world coordinates and its footprint.
pub struct Placed {
    pub name: String,
    pub palette: Vec<BlockState>,
    /// World (x, y, z) and palette index.
    pub blocks: Vec<([i32; 3], u32)>,
    pub footprint: Vec<(f64, f64)>,
    /// World block bounds of blocks and footprint: [min_x, min_z, max_x, max_z].
    pub bounds: [i32; 4],
}

/// Loads, rotates and positions every landmark.
pub fn place_all(
    root: &Path,
    cfg: &LandmarksConfig,
    origin: &Origin,
    height: &HeightMapping,
) -> Result<Vec<Placed>> {
    cfg.landmark.iter().map(|l| place(root, l, origin, height)).collect()
}

pub fn place(root: &Path, l: &Landmark, origin: &Origin, height: &HeightMapping) -> Result<Placed> {
    let s =
        load_schematic(&root.join(&l.schematic)).with_context(|| format!("landmark {}", l.name))?;
    let q = l.rotation / 90;
    let palette: Vec<BlockState> = s.palette.iter().map(|b| rotate_state(b, q)).collect();
    let (ax, az) = origin.block_at(l.anchor[0], l.anchor[1]);
    let ay = height.to_y(l.anchor[2]).y;
    let blocks: Vec<([i32; 3], u32)> = s
        .blocks
        .iter()
        .map(|&([x, y, z], i)| {
            let (dx, dz) = (x - l.origin[0], z - l.origin[2]);
            // Clockwise quarter turn: east → south, i.e. (x, z) → (−z, x).
            let (rx, rz) = match q {
                1 => (-dz, dx),
                2 => (-dx, -dz),
                3 => (dz, -dx),
                _ => (dx, dz),
            };
            ([ax + rx, ay + y - l.origin[1], az + rz], i)
        })
        .collect();
    let footprint: Vec<(f64, f64)> = l.footprint.iter().map(|p| (p[0], p[1])).collect();
    let mut bounds = [i32::MAX, i32::MAX, i32::MIN, i32::MIN];
    let mut grow = |x: i32, z: i32| {
        bounds = [bounds[0].min(x), bounds[1].min(z), bounds[2].max(x), bounds[3].max(z)];
    };
    for (p, _) in &blocks {
        grow(p[0], p[2]);
    }
    for &(e, n) in &footprint {
        let (x, z) = origin.block_at(e, n);
        grow(x, z);
    }
    Ok(Placed { name: l.name.clone(), palette, blocks, footprint, bounds })
}

/// A tile's landmark blocks, in the same shape as the other stage layers.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct LandmarkLayer {
    pub palette: Vec<BlockState>,
    /// (x, z, y0, y1) cleared to air (footprints above the terrain and its water).
    pub clears: Vec<(u16, u16, i16, i16)>,
    /// (x, y, z, palette index), tile-local.
    pub blocks: Vec<(u16, i16, u16, u16)>,
    /// Names of the landmarks in this tile.
    pub names: Vec<String>,
}

pub fn landmark_layer(placed: &[Placed], g: &GroundModel, origin: &Origin) -> LandmarkLayer {
    let t: TileKey = g.key;
    let (bx, bz) = t.min_block();
    let size = TILE_SIZE;
    let mut layer = LandmarkLayer::default();
    for p in placed {
        if p.bounds[2] < bx
            || p.bounds[0] >= bx + size
            || p.bounds[3] < bz
            || p.bounds[1] >= bz + size
        {
            continue;
        }
        layer.names.push(p.name.clone());
        // Footprint: cleared above the terrain or its water surface.
        let (min_e, max_n) =
            ((origin.e0 + i64::from(bx)) as f64, (origin.n0 - i64::from(bz)) as f64);
        rasterize_polygon(
            std::slice::from_ref(&p.footprint),
            min_e,
            max_n,
            size as usize,
            size as usize,
            |c, r| {
                let i = g.idx(c, r);
                let surface = if g.water_top[i] != NO_WATER { g.water_top[i] } else { g.ground[i] };
                layer.clears.push((c as u16, r as u16, surface + 1, (MAX_Y_EXCL - 1) as i16));
            },
        );
        let base = layer.palette.len() as u32;
        layer.palette.extend(p.palette.iter().cloned());
        for &([x, y, z], i) in &p.blocks {
            let (lx, lz) = (x - bx, z - bz);
            if (0..size).contains(&lx) && (0..size).contains(&lz) {
                layer.blocks.push((lx as u16, y as i16, lz as u16, (base + i) as u16));
            }
        }
    }
    layer
}

#[cfg(test)]
mod tests;
