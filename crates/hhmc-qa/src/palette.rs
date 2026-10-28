//! `hhmc palette`: average color of every block, derived locally from the pinned client
//! jar and written to `config/block_colors.toml`. Only these averages are committed; the
//! textures themselves never leave `tools/minecraft/`.
//!
//! Resolution follows the game's own assets: `blockstates/<id>.json` → first variant's
//! model → `models/…` with parents → the first texture among the keys in
//! [`TEXTURE_KEYS`] (top-down view first) → `textures/….png`, averaged over its first
//! animation frame, alpha-weighted. Tinted models are multiplied by the plains colors:
//! grass and foliage sampled from `colormap/grass.png` / `foliage.png` at the biome's
//! temperature and downfall, water from the biome's `effects.water_color`.

use std::collections::{BTreeMap, HashMap};
use std::io::Read;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde_json::Value;
use tracing::info;

const TEXTURE_KEYS: [&str; 10] =
    ["top", "end", "up", "all", "texture", "cross", "plant", "side", "pattern", "particle"];

pub type Rgb = [u8; 3];

struct Jar {
    zip: zip::ZipArchive<std::fs::File>,
}

impl Jar {
    fn read(&mut self, name: &str) -> Option<Vec<u8>> {
        let mut f = self.zip.by_name(name).ok()?;
        let mut b = Vec::new();
        f.read_to_end(&mut b).ok()?;
        Some(b)
    }

    fn json(&mut self, name: &str) -> Option<Value> {
        serde_json::from_slice(&self.read(name)?).ok()
    }
}

fn strip_ns(s: &str) -> &str {
    s.strip_prefix("minecraft:").unwrap_or(s)
}

/// Decodes a PNG to RGBA8 and returns its first square frame.
fn rgba(png_bytes: &[u8]) -> Result<(usize, usize, Vec<u8>)> {
    let mut dec = png::Decoder::new(std::io::Cursor::new(png_bytes));
    dec.set_transformations(
        png::Transformations::EXPAND | png::Transformations::STRIP_16 | png::Transformations::ALPHA,
    );
    let mut r = dec.read_info()?;
    let mut buf = vec![0; r.output_buffer_size().context("png size")?];
    let info = r.next_frame(&mut buf)?;
    let (w, h) = (info.width as usize, info.height as usize);
    let ch = info.color_type.samples();
    let mut out = Vec::with_capacity(w * h * 4);
    for px in buf[..w * h * ch].chunks(ch) {
        match ch {
            4 => out.extend_from_slice(px),
            2 => out.extend_from_slice(&[px[0], px[0], px[0], px[1]]),
            3 => out.extend_from_slice(&[px[0], px[1], px[2], 255]),
            _ => out.extend_from_slice(&[px[0], px[0], px[0], 255]),
        }
    }
    Ok((w, h.min(w), out))
}

fn average(png_bytes: &[u8]) -> Result<Option<Rgb>> {
    let (w, h, px) = rgba(png_bytes)?;
    let (mut r, mut g, mut b, mut a) = (0f64, 0f64, 0f64, 0f64);
    for p in px[..w * h * 4].chunks(4) {
        let al = f64::from(p[3]) / 255.0;
        r += f64::from(p[0]) * al;
        g += f64::from(p[1]) * al;
        b += f64::from(p[2]) * al;
        a += al;
    }
    if a < 1.0 {
        return Ok(None);
    }
    Ok(Some([(r / a).round() as u8, (g / a).round() as u8, (b / a).round() as u8]))
}

fn hex(c: Rgb) -> String {
    format!("#{:02x}{:02x}{:02x}", c[0], c[1], c[2])
}

fn parse_hex(s: &str) -> Option<Rgb> {
    let s = s.strip_prefix('#')?;
    let v = u32::from_str_radix(s, 16).ok()?;
    Some([(v >> 16) as u8, (v >> 8) as u8, v as u8])
}

fn multiply(a: Rgb, t: Rgb) -> Rgb {
    [0, 1, 2].map(|i| ((u16::from(a[i]) * u16::from(t[i])) / 255) as u8)
}

/// Color of a colormap at a biome's temperature and downfall (vanilla lookup).
fn colormap(jar: &mut Jar, name: &str, temperature: f64, downfall: f64) -> Result<Rgb> {
    let bytes =
        jar.read(&format!("assets/minecraft/textures/colormap/{name}.png")).context("colormap")?;
    let (w, _, px) = rgba(&bytes)?;
    let t = temperature.clamp(0.0, 1.0);
    let d = downfall.clamp(0.0, 1.0) * t;
    let x = ((1.0 - t) * 255.0) as usize;
    let y = ((1.0 - d) * 255.0) as usize;
    let i = (y * w + x) * 4;
    Ok([px[i], px[i + 1], px[i + 2]])
}

struct Model {
    textures: HashMap<String, String>,
    tinted: bool,
}

fn load_model(jar: &mut Jar, name: &str, depth: usize) -> Option<Model> {
    if depth > 16 {
        return None;
    }
    let name = strip_ns(name);
    let v = jar.json(&format!("assets/minecraft/models/{name}.json"))?;
    let mut m = match v.get("parent").and_then(Value::as_str) {
        Some(p) if !p.starts_with("builtin/") => load_model(jar, p, depth + 1)?,
        _ => Model { textures: HashMap::new(), tinted: false },
    };
    if let Some(t) = v.get("textures").and_then(Value::as_object) {
        for (k, val) in t {
            // Since 26.x a texture may also be an object: {"sprite": "...", ...}.
            let s = val.as_str().or_else(|| val.get("sprite").and_then(Value::as_str));
            if let Some(s) = s {
                m.textures.insert(k.clone(), s.to_owned());
            }
        }
    }
    if let Some(els) = v.get("elements").and_then(Value::as_array) {
        m.tinted = els.iter().any(|e| {
            e.get("faces")
                .and_then(Value::as_object)
                .is_some_and(|f| f.values().any(|face| face.get("tintindex").is_some()))
        });
    }
    Some(m)
}

fn resolve(textures: &HashMap<String, String>, key: &str) -> Option<String> {
    let mut v = textures.get(key)?.clone();
    for _ in 0..8 {
        match v.strip_prefix('#') {
            Some(k) => v = textures.get(k)?.clone(),
            None => return Some(v),
        }
    }
    None
}

fn first_model(state: &Value) -> Option<String> {
    let pick = |v: &Value| -> Option<String> {
        let m = if let Some(a) = v.as_array() { a.first()? } else { v };
        m.get("model").and_then(Value::as_str).map(str::to_owned)
    };
    if let Some(vars) = state.get("variants").and_then(Value::as_object) {
        let v = vars.get("").or_else(|| vars.values().next())?;
        return pick(v);
    }
    state.get("multipart")?.as_array()?.iter().find_map(|p| pick(p.get("apply")?))
}

pub fn compute(client_jar: &Path) -> Result<BTreeMap<String, Rgb>> {
    let mut jar = Jar { zip: zip::ZipArchive::new(std::fs::File::open(client_jar)?)? };
    let plains = jar.json("data/minecraft/worldgen/biome/plains.json").context("plains biome")?;
    let t = plains["temperature"].as_f64().context("plains temperature")?;
    let d = plains["downfall"].as_f64().context("plains downfall")?;
    let water = plains["effects"]["water_color"]
        .as_str()
        .and_then(parse_hex)
        .context("plains water_color")?;
    let grass = colormap(&mut jar, "grass", t, d)?;
    let foliage = colormap(&mut jar, "foliage", t, d)?;
    info!(grass = hex(grass), foliage = hex(foliage), water = hex(water), "plains tints");

    let names: Vec<String> = jar
        .zip
        .file_names()
        .filter_map(|n| n.ok())
        .filter_map(|n| {
            n.strip_prefix("assets/minecraft/blockstates/")
                .and_then(|s| s.strip_suffix(".json"))
                .map(str::to_owned)
        })
        .collect();
    let mut out = BTreeMap::new();
    for id in names {
        let Some(state) = jar.json(&format!("assets/minecraft/blockstates/{id}.json")) else {
            continue;
        };
        let Some(model) = first_model(&state).and_then(|m| load_model(&mut jar, &m, 0)) else {
            continue;
        };
        let tex = TEXTURE_KEYS.iter().find_map(|k| resolve(&model.textures, k));
        let Some(tex) = tex else { continue };
        let path = format!("assets/minecraft/textures/{}.png", strip_ns(&tex));
        let Some(bytes) = jar.read(&path) else { continue };
        let Some(mut c) = average(&bytes)? else { continue };
        if id == "water" || id == "bubble_column" {
            c = multiply(c, water);
        } else if model.tinted {
            c = multiply(c, if id.contains("leaves") || id == "vine" { foliage } else { grass });
        }
        out.insert(format!("minecraft:{id}"), c);
    }
    if !out.contains_key("minecraft:stone") || !out.contains_key("minecraft:water") {
        bail!("palette is missing basic blocks; the asset layout may have changed");
    }
    Ok(out)
}

pub fn write_toml(path: &Path, version: &str, colors: &BTreeMap<String, Rgb>) -> Result<()> {
    let mut s = format!(
        "# Average color per block, derived by `hhmc palette` from the Minecraft {version} client jar\n\
         # (top texture, alpha-weighted, plains tints). Derived data only; no textures.\n\n[color]\n"
    );
    for (k, c) in colors {
        s.push_str(&format!("\"{k}\" = \"{}\"\n", hex(*c)));
    }
    std::fs::write(path, s)?;
    Ok(())
}

/// Loads `config/block_colors.toml`.
pub fn load(path: &Path) -> Result<HashMap<String, Rgb>> {
    #[derive(serde::Deserialize)]
    struct F {
        color: HashMap<String, String>,
    }
    let f: F = hhmc_core::config::load_toml(path)?;
    f.color
        .into_iter()
        .map(|(k, v)| {
            parse_hex(&v).map(|c| (k.clone(), c)).with_context(|| format!("color of {k}"))
        })
        .collect()
}
