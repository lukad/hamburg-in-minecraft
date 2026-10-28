//! `hhmc map`: the top-down web map in out/map/.
//!
//! - `render/<rx>_<rz>.png`: each tile's top surface at 1 px per block (palette colors,
//!   water tinted by depth, hillshade), from the region files.
//! - `heights/<rx>_<rz>.png`: the highest block of every column, (y + 64) in red (high
//!   byte) and green (low byte), for the `/tp` command a click copies.
//! - `tiles/{z}/{x}/{y}.png`: XYZ tiles in Web Mercator. The highest zoom samples the
//!   renders at each pixel's block (PROJ for a 9 × 9 grid of control points per tile, bilinear
//!   in between); lower zooms average their four children.
//! - `index.html`: MapLibre GL JS with a slider that blends to the LGV's DOP WMS and a
//!   click that copies `/tp <x> <y> <z>`.

use std::collections::{BTreeSet, HashMap};
use std::f64::consts::PI;
use std::path::Path;

use anyhow::{Context, Result};
use hhmc_core::{Paths, TILE_SIZE, TileKey, WorldConfig};
use rayon::prelude::*;
use serde::Deserialize;
use tracing::info;

const TILE_PX: usize = 256;
/// Control points per tile side (every 32 pixels, corners included).
const CONTROL: usize = 9;

#[derive(Deserialize)]
struct MapConfig {
    min_zoom: u32,
    max_zoom: u32,
    wms: Wms,
    libs: Libs,
}

#[derive(Deserialize)]
struct Wms {
    url: String,
    layer: String,
    source: String,
}

#[derive(Deserialize)]
struct Libs {
    maplibre_js: String,
    maplibre_css: String,
    proj4_js: String,
    epsg25832: String,
}

/// Longitude and latitude (degrees) of global pixel position (`px`, `py`) at zoom `z`.
fn mercator_to_lonlat(px: f64, py: f64, z: u32) -> (f64, f64) {
    let n = (TILE_PX as f64) * f64::from(1u32 << z);
    let lon = px / n * 360.0 - 180.0;
    let lat = (PI * (1.0 - 2.0 * py / n)).sinh().atan().to_degrees();
    (lon, lat)
}

/// XYZ tile containing (`lon`, `lat`) at zoom `z`.
fn lonlat_to_tile(lon: f64, lat: f64, z: u32) -> (u32, u32) {
    let n = f64::from(1u32 << z);
    let x = ((lon + 180.0) / 360.0 * n).floor();
    let r = lat.to_radians();
    let y = ((1.0 - (r.tan() + 1.0 / r.cos()).ln() / PI) / 2.0 * n).floor();
    (x as u32, y as u32)
}

pub fn map(paths: &Paths, tiles: &[TileKey]) -> Result<()> {
    let start = std::time::Instant::now();
    let world = WorldConfig::load(paths)?;
    let cfg: MapConfig = hhmc_core::config::load_toml(&paths.config().join("map.toml"))?;
    let colors = hhmc_qa::palette::load(&paths.config().join("block_colors.toml"))?;
    let out = paths.out().join("map");
    let layout = hhmc_world::WorldLayout::new(paths.out().join("world"));
    for d in ["render", "heights", "tiles"] {
        std::fs::create_dir_all(out.join(d))?;
    }
    // 1. Renders and heights per region tile.
    let renders: HashMap<(i32, i32), Vec<u8>> = tiles
        .par_iter()
        .map(|&t| -> Result<_> {
            let vt = hhmc_world::read_tile(&layout.region_file(t), t)
                .with_context(|| format!("tile {t}: build it first"))?;
            let (rgb, tops) = hhmc_qa::ground_qa::top_view(&vt, &colors);
            let size = TILE_SIZE as usize;
            hhmc_qa::ground_qa::write_png(
                &out.join("render").join(format!("{}_{}.png", t.rx, t.rz)),
                size,
                size,
                &rgb,
            )?;
            let mut h = vec![0u8; size * size * 3];
            for (k, &y) in tops.iter().enumerate() {
                let v = (y - hhmc_core::voxel::MIN_Y).clamp(0, 65535) as u16;
                h[k * 3] = (v >> 8) as u8;
                h[k * 3 + 1] = (v & 0xff) as u8;
            }
            hhmc_qa::ground_qa::write_png(
                &out.join("heights").join(format!("{}_{}.png", t.rx, t.rz)),
                size,
                size,
                &h,
            )?;
            // Empty columns (outside a clipped area's boundary) stay transparent.
            let rgba: Vec<u8> = rgb
                .chunks(3)
                .zip(&tops)
                .flat_map(|(c, &y)| {
                    [c[0], c[1], c[2], if y < hhmc_core::voxel::MIN_Y { 0 } else { 255 }]
                })
                .collect();
            Ok(((t.rx, t.rz), rgba))
        })
        .collect::<Result<_>>()?;
    info!(tiles = renders.len(), "tile renders written");
    // 2. The area in WGS84 (densified tile outlines) and the XYZ tiles over it.
    let (e0, n0) = (world.origin.e0 as f64, world.origin.n0 as f64);
    let mut outline = Vec::new();
    for t in tiles {
        let (bx, bz) = t.min_block();
        for i in 0..=8 {
            let s = f64::from(TILE_SIZE) * f64::from(i) / 8.0;
            let (x0, z0) = (f64::from(bx), f64::from(bz));
            let x1 = x0 + f64::from(TILE_SIZE);
            let z1 = z0 + f64::from(TILE_SIZE);
            for (x, z) in [(x0 + s, z0), (x0 + s, z1), (x0, z0 + s), (x1, z0 + s)] {
                outline.push((e0 + x, n0 - z));
            }
        }
    }
    let ll = hhmc_core::proj::epsg25832_to_wgs84(&outline)?;
    let (mut west, mut east, mut south, mut north) = (f64::MAX, f64::MIN, f64::MAX, f64::MIN);
    for &(lat, lon) in &ll {
        (west, east, south, north) = (west.min(lon), east.max(lon), south.min(lat), north.max(lat));
    }
    let zmax = cfg.max_zoom;
    let (tx0, ty0) = lonlat_to_tile(west, north, zmax);
    let (tx1, ty1) = lonlat_to_tile(east, south, zmax);
    let xyz: Vec<(u32, u32)> = (ty0..=ty1).flat_map(|y| (tx0..=tx1).map(move |x| (x, y))).collect();
    // Control points of every tile, projected in one PROJ call.
    let mut pts = Vec::with_capacity(xyz.len() * CONTROL * CONTROL);
    for &(x, y) in &xyz {
        for j in 0..CONTROL {
            for i in 0..CONTROL {
                let step = (TILE_PX / (CONTROL - 1)) as f64;
                let (lon, lat) = mercator_to_lonlat(
                    f64::from(x) * TILE_PX as f64 + i as f64 * step,
                    f64::from(y) * TILE_PX as f64 + j as f64 * step,
                    zmax,
                );
                pts.push((lat, lon));
            }
        }
    }
    let utm = hhmc_core::proj::wgs84_to_25832(&pts)?;
    // 3. The highest zoom: each pixel shows the block under its center.
    let tile_dir = out.join("tiles");
    let written: Vec<(u32, u32)> = xyz
        .par_iter()
        .enumerate()
        .filter_map(|(k, &(x, y))| {
            let cp = &utm[k * CONTROL * CONTROL..(k + 1) * CONTROL * CONTROL];
            let step = (TILE_PX / (CONTROL - 1)) as f64;
            let mut img = vec![0u8; TILE_PX * TILE_PX * 4];
            let mut any = false;
            for py in 0..TILE_PX {
                for px in 0..TILE_PX {
                    let (fx, fy) = ((px as f64 + 0.5) / step, (py as f64 + 0.5) / step);
                    let (i, j) = ((fx as usize).min(CONTROL - 2), (fy as usize).min(CONTROL - 2));
                    let (u, v) = (fx - i as f64, fy - j as f64);
                    let at = |a: usize, b: usize| cp[b * CONTROL + a];
                    let lerp = |p: (f64, f64), q: (f64, f64), t: f64| {
                        (p.0 + (q.0 - p.0) * t, p.1 + (q.1 - p.1) * t)
                    };
                    let top = lerp(at(i, j), at(i + 1, j), u);
                    let bottom = lerp(at(i, j + 1), at(i + 1, j + 1), u);
                    let (e, n) = lerp(top, bottom, v);
                    let (bx, bz) = ((e - e0).floor() as i32, (n0 - n).floor() as i32);
                    let key = (bx.div_euclid(TILE_SIZE), bz.div_euclid(TILE_SIZE));
                    let Some(r) = renders.get(&key) else { continue };
                    let (lx, lz) = (bx.rem_euclid(TILE_SIZE), bz.rem_euclid(TILE_SIZE));
                    let s = (lz as usize * TILE_SIZE as usize + lx as usize) * 4;
                    if r[s + 3] == 0 {
                        continue;
                    }
                    let d = (py * TILE_PX + px) * 4;
                    img[d..d + 4].copy_from_slice(&r[s..s + 4]);
                    any = true;
                }
            }
            any.then(|| {
                write_rgba_png(&tile_dir.join(format!("{zmax}/{x}/{y}.png")), &img).map(|()| (x, y))
            })
        })
        .collect::<Result<_>>()?;
    info!(zoom = zmax, tiles = written.len(), "map tiles written");
    // 4. Lower zooms: each tile averages its four children.
    let mut level: BTreeSet<(u32, u32)> = written.into_iter().collect();
    let mut total = level.len();
    for z in (cfg.min_zoom..zmax).rev() {
        let parents: BTreeSet<(u32, u32)> = level.iter().map(|&(x, y)| (x / 2, y / 2)).collect();
        let done: Vec<(u32, u32)> = parents
            .par_iter()
            .map(|&(x, y)| -> Result<(u32, u32)> {
                let mut img = vec![0u8; TILE_PX * TILE_PX * 4];
                for (cx, cy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
                    let p = tile_dir.join(format!("{}/{}/{}.png", z + 1, x * 2 + cx, y * 2 + cy));
                    if !level.contains(&(x * 2 + cx, y * 2 + cy)) {
                        continue;
                    }
                    let child = read_rgba_png(&p)?;
                    let half = TILE_PX / 2;
                    for py in 0..half {
                        for px in 0..half {
                            let (mut acc, mut a) = ([0u32; 3], 0u32);
                            for (dx, dy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
                                let s = ((py * 2 + dy) * TILE_PX + px * 2 + dx) * 4;
                                let w = u32::from(child[s + 3]);
                                for c in 0..3 {
                                    acc[c] += u32::from(child[s + c]) * w;
                                }
                                a += w;
                            }
                            if a == 0 {
                                continue;
                            }
                            let d =
                                ((cy as usize * half + py) * TILE_PX + cx as usize * half + px) * 4;
                            for c in 0..3 {
                                img[d + c] = (acc[c] / a) as u8;
                            }
                            img[d + 3] = (a / 4) as u8;
                        }
                    }
                }
                write_rgba_png(&tile_dir.join(format!("{z}/{x}/{y}.png")), &img)?;
                Ok((x, y))
            })
            .collect::<Result<_>>()?;
        total += done.len();
        level = done.into_iter().collect();
    }
    // 5. The page.
    let sources = hhmc_fetch::Sources::load(&paths.sources_toml())?;
    let dop = sources.get(&cfg.wms.source).context("map.toml: unknown wms.source")?;
    let attribution = std::fs::read_to_string(paths.root.join("ATTRIBUTION.md")).ok();
    if let Some(a) = &attribution {
        std::fs::write(out.join("ATTRIBUTION.md"), a)?;
    }
    let page = page(&cfg, &world, [west, south, east, north], &dop.attribution, &dop.license);
    std::fs::write(out.join("index.html"), page)?;
    info!(
        dir = %out.display(),
        xyz_tiles = total,
        secs = format!("{:.1}", start.elapsed().as_secs_f64()),
        "web map written; serve it with any static file server, e.g. `python3 -m http.server -d out/map`"
    );
    Ok(())
}

fn write_rgba_png(path: &Path, rgba: &[u8]) -> Result<()> {
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    let f = std::io::BufWriter::new(std::fs::File::create(path)?);
    let mut enc = png::Encoder::new(f, TILE_PX as u32, TILE_PX as u32);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header()?.write_image_data(rgba)?;
    Ok(())
}

fn read_rgba_png(path: &Path) -> Result<Vec<u8>> {
    let dec = png::Decoder::new(std::io::BufReader::new(std::fs::File::open(path)?));
    let mut rd = dec.read_info()?;
    let mut buf = vec![0; rd.output_buffer_size().context("png size")?];
    rd.next_frame(&mut buf)?;
    Ok(buf)
}

fn js_str(s: &str) -> String {
    serde_json::to_string(s).expect("string")
}

fn page(
    cfg: &MapConfig,
    world: &WorldConfig,
    bounds: [f64; 4],
    dop_attr: &str,
    dop_license: &str,
) -> String {
    let wms = format!(
        "{}?SERVICE=WMS&VERSION=1.3.0&REQUEST=GetMap&LAYERS={}&STYLES=&CRS=EPSG:3857&BBOX={{bbox-epsg-3857}}&WIDTH=256&HEIGHT=256&FORMAT=image/jpeg",
        cfg.wms.url, cfg.wms.layer
    );
    let config = serde_json::json!({
        "e0": world.origin.e0,
        "n0": world.origin.n0,
        "tile": TILE_SIZE,
        "minY": hhmc_core::voxel::MIN_Y,
        "minZoom": cfg.min_zoom,
        "maxZoom": cfg.max_zoom,
        "bounds": bounds,
        "wms": wms,
        "dopAttribution": format!("Luftbild © {dop_attr} ({dop_license})"),
        "epsg25832": cfg.libs.epsg25832,
    });
    format!(
        r##"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Hamburg 1:1 map</title>
<link rel="stylesheet" href={css}>
<script src={maplibre}></script>
<script src={proj4}></script>
<style>
:root {{ --bg:#fff; --fg:#1d1d1f; --mute:#666; --card:rgba(255,255,255,.92); --line:#ddd; }}
@media (prefers-color-scheme: dark) {{ :root {{ --bg:#161616; --fg:#eee; --mute:#aaa; --card:rgba(30,30,30,.92); --line:#444; }} }}
html, body {{ margin:0; height:100%; background:var(--bg); color:var(--fg); font:14px/1.4 system-ui, sans-serif; }}
#map {{ position:absolute; inset:0; }}
.panel {{ position:absolute; top:10px; left:10px; z-index:2; background:var(--card); border:1px solid var(--line);
  border-radius:8px; padding:10px 12px; max-width:min(320px, calc(100vw - 44px)); box-shadow:0 2px 8px rgba(0,0,0,.2); }}
.panel h1 {{ font-size:15px; margin:0 0 6px; }}
.panel label {{ display:flex; gap:8px; align-items:center; }}
.panel input[type=range] {{ flex:1; }}
.panel .hint {{ color:var(--mute); margin:6px 0 0; font-size:12px; }}
#tp {{ margin-top:8px; display:none; }}
#tp code {{ display:block; padding:4px 6px; background:var(--bg); border:1px solid var(--line); border-radius:4px; user-select:all; word-break:break-all; }}
</style>
</head>
<body>
<div id="map"></div>
<div class="panel">
  <h1>Hamburg 1:1</h1>
  <label>World <input id="blend" type="range" min="0" max="100" value="0" aria-label="Blend from the world to the aerial photo"> Aerial</label>
  <p class="hint">Click a spot to copy its <code>/tp</code> command. Sources: <a href="ATTRIBUTION.md">attribution</a>.</p>
  <div id="tp"><code id="tpcmd"></code><span class="hint" id="tpnote"></span></div>
</div>
<script>
const C = {config};
proj4.defs("EPSG:25832", C.epsg25832);
const map = new maplibregl.Map({{
  container: "map",
  bounds: [[C.bounds[0], C.bounds[1]], [C.bounds[2], C.bounds[3]]],
  maxZoom: C.maxZoom + 4,
  style: {{
    version: 8,
    sources: {{
      dop: {{ type: "raster", tiles: [C.wms], tileSize: 256, attribution: C.dopAttribution }},
      world: {{ type: "raster", tiles: ["tiles/{{z}}/{{x}}/{{y}}.png"], tileSize: 256,
        minzoom: C.minZoom, maxzoom: C.maxZoom, bounds: C.bounds,
        attribution: "World: Hamburg 1:1 (<a href=\"ATTRIBUTION.md\">data sources</a>)" }}
    }},
    layers: [
      {{ id: "bg", type: "background", paint: {{ "background-color": "#202020" }} }},
      {{ id: "dop", type: "raster", source: "dop" }},
      {{ id: "world", type: "raster", source: "world", paint: {{ "raster-resampling": "nearest", "raster-fade-duration": 0 }} }}
    ]
  }}
}});
map.addControl(new maplibregl.NavigationControl(), "top-right");
map.addControl(new maplibregl.ScaleControl(), "bottom-left");
const blend = document.getElementById("blend");
blend.addEventListener("input", () => map.setPaintProperty("world", "raster-opacity", 1 - blend.value / 100));
// Heights: one PNG per region tile, (y - minY) in red (high byte) and green (low byte).
const heights = new Map();
function heightTile(rx, rz) {{
  const key = rx + "_" + rz;
  if (!heights.has(key)) {{
    heights.set(key, new Promise(resolve => {{
      const img = new Image();
      img.onload = () => {{
        const c = document.createElement("canvas");
        c.width = img.width; c.height = img.height;
        const g = c.getContext("2d", {{ willReadFrequently: true }});
        g.drawImage(img, 0, 0);
        resolve(g.getImageData(0, 0, img.width, img.height).data);
      }};
      img.onerror = () => resolve(null);
      img.src = "heights/" + key + ".png";
    }}));
  }}
  return heights.get(key);
}}
map.on("click", async e => {{
  const [east, north] = proj4("EPSG:4326", "EPSG:25832", [e.lngLat.lng, e.lngLat.lat]);
  const x = Math.floor(east - C.e0), z = Math.floor(C.n0 - north);
  const rx = Math.floor(x / C.tile), rz = Math.floor(z / C.tile);
  const data = await heightTile(rx, rz);
  const box = document.getElementById("tp"), cmd = document.getElementById("tpcmd"), note = document.getElementById("tpnote");
  box.style.display = "block";
  if (!data) {{ cmd.textContent = "outside the built area"; note.textContent = ""; return; }}
  const k = ((z - rz * C.tile) * C.tile + (x - rx * C.tile)) * 4;
  const top = data[k] * 256 + data[k + 1] + C.minY;
  const text = `/tp ${{x}} ${{top + 1}} ${{z}}`;
  cmd.textContent = text;
  try {{ await navigator.clipboard.writeText(text); note.textContent = "Copied."; }}
  catch {{ note.textContent = "Select it to copy."; }}
}});
</script>
</body>
</html>
"##,
        css = js_str(&cfg.libs.maplibre_css),
        maplibre = js_str(&cfg.libs.maplibre_js),
        proj4 = js_str(&cfg.libs.proj4_js),
        config = config,
    )
}
