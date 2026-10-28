//! Golden tiles: 64 × 64 m clips of real prepared inputs in `tests/golden/<name>/`, built
//! and checked from a clean temporary root. The repository holds each clip's `golden.toml`
//! and expectations; its data (`data/`) is cut locally from a full build's prepared inputs
//! (`hhmc golden extract --name <name>`), and `hhmc golden check` skips clips without it.
//!
//! `hhmc golden extract` keeps one tile's prepared inputs within the window plus the tile
//! buffer (32 m) and fills the rest of the tile with flat, neutral values (DGM1 at the
//! window's median height, default land use, no water, gray orthophoto); buildings,
//! structures and trees are kept where they reach into that area (buildings where they
//! reach into the window), with the LoD3 textures and the cadaster trees they need. `hhmc golden check` copies the repository's config
//! and a golden tile's data into `out/golden/<name>/`, runs the stages and `hhmc qa` on
//! the tile, and compares the block counts in the window and its top-down render with the
//! committed expectations (`--update` rewrites them).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use hhmc_core::raster::{
    NODATA, read_f32, read_rgba, read_u8, read_u16, write_f32, write_rgba, write_u8, write_u16,
};
use hhmc_core::{Paths, TILE_BUFFER, TILE_SIZE, TileKey, WorldConfig};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

/// Context kept around the window (the tile buffer: stages see as far).
const MARGIN: i32 = TILE_BUFFER as i32;
/// A block's count may differ by this share of the larger count (or `MIN_COUNT_DIFF`).
const COUNT_TOLERANCE: f64 = 0.05;
const MIN_COUNT_DIFF: u64 = 16;
/// Share of window pixels of the top-down render allowed to differ visibly.
const PIXEL_TOLERANCE: f64 = 0.02;

#[derive(Serialize, Deserialize)]
struct GoldenToml {
    /// What the clip shows and why it is a golden tile.
    description: String,
    tile: [i32; 2],
    /// Tile-local block window [x0, z0, x1, z1) (64 × 64).
    window: [i32; 4],
}

#[derive(Serialize, Deserialize, PartialEq)]
struct Expected {
    /// Whether `hhmc qa` passes on the tile.
    qa_passes: bool,
    /// Blocks by state name within the window, all heights.
    counts: BTreeMap<String, u64>,
}

fn golden_dir(paths: &Paths) -> PathBuf {
    paths.root.join("tests/golden")
}

/// Tile-local rectangle kept with real data.
fn kept(window: [i32; 4]) -> [i32; 4] {
    let lo = -(TILE_BUFFER as i32);
    let hi = TILE_SIZE + TILE_BUFFER as i32;
    [
        (window[0] - MARGIN).max(lo),
        (window[1] - MARGIN).max(lo),
        (window[2] + MARGIN).min(hi),
        (window[3] + MARGIN).min(hi),
    ]
}

/// The kept rectangle in EPSG:25832: [min_e, min_n, max_e, max_n].
fn kept_world(cfg: &WorldConfig, t: TileKey, window: [i32; 4]) -> [f64; 4] {
    let k = kept(window);
    let (bx, bz) = t.min_block();
    let e = |x: i32| (cfg.origin.e0 + i64::from(bx + x)) as f64;
    let n = |z: i32| (cfg.origin.n0 - i64::from(bz + z)) as f64;
    [e(k[0]), n(k[3]), e(k[2]), n(k[1])]
}

fn inside(r: [f64; 4], e: f64, n: f64) -> bool {
    e >= r[0] && e <= r[2] && n >= r[1] && n <= r[3]
}

fn rings_touch(r: [f64; 4], rings: &[Vec<(f64, f64)>]) -> bool {
    let pts = rings.iter().flatten();
    let (mut a, mut b, mut c, mut d) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
    for &(e, n) in pts {
        (a, b, c, d) = (a.min(e), b.min(n), c.max(e), d.max(n));
    }
    a <= r[2] && c >= r[0] && b <= r[3] && d >= r[1]
}

pub fn extract(
    paths: &Paths,
    name: &str,
    t: TileKey,
    origin_xz: [i32; 2],
    description: &str,
) -> Result<()> {
    let cfg = WorldConfig::load(paths)?;
    let window = [origin_xz[0], origin_xz[1], origin_xz[0] + 64, origin_xz[1] + 64];
    ensure!(
        window[0] >= 0 && window[1] >= 0 && window[2] <= TILE_SIZE && window[3] <= TILE_SIZE,
        "the window must lie inside the tile"
    );
    let dir = golden_dir(paths).join(name);
    ensure!(!dir.join("data").exists(), "{} exists", dir.join("data").display());
    let prep = |id: &str| paths.prepared().join(id).join(format!("{}_{}.tif", t.rx, t.rz));
    let out = |id: &str| dir.join("data/prepared").join(id).join(format!("{}_{}.tif", t.rx, t.rz));
    // Buildings reaching into the window (each fills only its own columns), with their
    // textures.
    let wr = {
        let (bx, bz) = t.min_block();
        let e = |x: i32| (cfg.origin.e0 + i64::from(bx + x)) as f64;
        let n = |z: i32| (cfg.origin.n0 - i64::from(bz + z)) as f64;
        [e(window[0]) - 2.0, n(window[3]) - 2.0, e(window[2]) + 2.0, n(window[1]) + 2.0]
    };
    let bin = paths.prepared().join("buildings").join(format!("{}_{}.bin", t.rx, t.rz));
    let meshes: Vec<_> = if !bin.exists() {
        Vec::new()
    } else {
        hhmc_buildings::load_meshes(&bin)?
            .into_iter()
            .filter(|m| {
                m.bbox[0] <= wr[2] && m.bbox[3] >= wr[0] && m.bbox[1] <= wr[3] && m.bbox[4] >= wr[1]
            })
            .collect()
    };
    // Real data within the window plus the tile buffer, and under the kept buildings
    // (their roofs are checked against bDOM).
    let k = {
        let mut k = kept(window);
        let (bx, bz) = t.min_block();
        let lo = -(TILE_BUFFER as i32);
        let hi = TILE_SIZE + TILE_BUFFER as i32;
        for m in &meshes {
            let x0 = (m.bbox[0] - (cfg.origin.e0 + i64::from(bx)) as f64).floor() as i32 - 2;
            let x1 = (m.bbox[3] - (cfg.origin.e0 + i64::from(bx)) as f64).ceil() as i32 + 2;
            let z0 = ((cfg.origin.n0 - i64::from(bz)) as f64 - m.bbox[4]).floor() as i32 - 2;
            let z1 = ((cfg.origin.n0 - i64::from(bz)) as f64 - m.bbox[1]).ceil() as i32 + 2;
            k = [
                k[0].min(x0).max(lo),
                k[1].min(z0).max(lo),
                k[2].max(x1).min(hi),
                k[3].max(z1).min(hi),
            ];
        }
        k
    };
    let keep = |w: usize, col: usize, row: usize| {
        let b = (w as i32 - TILE_SIZE) / 2;
        let (x, z) = (col as i32 - b, row as i32 - b);
        x >= k[0] && x < k[2] && z >= k[1] && z < k[3]
    };
    // Rasters.
    let dgm = read_f32(&prep("dgm1"))?;
    let mut heights: Vec<f32> = (0..dgm.h)
        .flat_map(|r| (0..dgm.w).map(move |c| (c, r)))
        .filter(|&(c, r)| keep(dgm.w, c, r))
        .map(|(c, r)| dgm.get(c, r))
        .filter(|&v| v != NODATA)
        .collect();
    heights.sort_by(f32::total_cmp);
    let fill = heights.get(heights.len() / 2).copied().context("no DGM1 in the window")?;
    for (id, value) in [("dgm1", Some(fill)), ("dgmw", None), ("bdom1", Some(fill))] {
        let mut g = read_f32(&prep(id))?;
        for r in 0..g.h {
            for c in 0..g.w {
                if !keep(g.w, c, r) {
                    g.set(c, r, value.unwrap_or(NODATA));
                }
            }
        }
        write_f32(&out(id), &g, Some(f64::from(NODATA)))?;
    }
    for id in ["landuse", "landuse_alkis"] {
        let mut g = read_u8(&prep(id))?;
        for r in 0..g.h {
            for c in 0..g.w {
                if !keep(g.w, c, r) {
                    g.set(c, r, 0);
                }
            }
        }
        write_u8(&out(id), &g, None)?;
    }
    {
        let mut g = read_u16(&prep("water"))?;
        for r in 0..g.h {
            for c in 0..g.w {
                if !keep(g.w, c, r) {
                    g.set(c, r, 0);
                }
            }
        }
        write_u16(&out("water"), &g, None)?;
    }
    if prep("dop20").exists() {
        let mut g = read_rgba(&prep("dop20"))?;
        for r in 0..g.h {
            for c in 0..g.w {
                if !keep(g.w, c, r) {
                    g.set(c, r, [128, 128, 128, 255]);
                }
            }
        }
        write_rgba(&out("dop20"), &g)?;
    }
    let bodies = paths.prepared().join("water/bodies.json");
    copy(&bodies, &dir.join("data/prepared/water/bodies.json"))?;
    let rect = kept_world(&cfg, t, window);
    if bin.exists() {
        for m in &meshes {
            for p in &m.textures {
                let src = paths.root.join(p);
                if src.exists() {
                    copy(&src, &dir.join(p))?;
                }
            }
        }
        let path = dir.join("data/prepared/buildings").join(format!("{}_{}.bin", t.rx, t.rz));
        std::fs::create_dir_all(path.parent().expect("dir"))?;
        std::fs::write(&path, zstd::encode_all(&postcard::to_stdvec(&meshes)?[..], 3)?)?;
        info!(buildings = meshes.len(), "kept");
    }
    // Structures.
    let ti = hhmc_core::infra::TileInfra::path(paths, t);
    if ti.exists() {
        let mut s = hhmc_core::infra::TileInfra::load(&ti)?;
        s.decks.retain(|d| d.polygons.iter().any(|p| rings_touch(rect, p)));
        s.piers.retain(|d| d.polygons.iter().any(|p| rings_touch(rect, p)));
        s.tracks.retain(|tr| rings_touch(rect, std::slice::from_ref(&tr.line)));
        s.occluders.retain(|o| rings_touch(rect, o));
        let path = dir.join("data/prepared/infra").join(format!("{}_{}.json", t.rx, t.rz));
        std::fs::create_dir_all(path.parent().expect("dir"))?;
        std::fs::write(&path, serde_json::to_string(&s)? + "\n")?;
    }
    // Trees, and the cadaster entries they come from.
    let tt = hhmc_core::trees::TileTrees::path(paths, t);
    if tt.exists() {
        let mut s = hhmc_core::trees::TileTrees::load(&tt)?;
        s.own.retain(|x| inside(rect, x.e, x.n));
        s.near.retain(|x| inside(rect, x.e, x.n));
        s.conflicts.retain(|x| inside(rect, x.e, x.n));
        let path = dir.join("data/prepared/trees").join(format!("{}_{}.json", t.rx, t.rz));
        std::fs::create_dir_all(path.parent().expect("dir"))?;
        std::fs::write(&path, serde_json::to_string(&s)? + "\n")?;
    }
    let sources = hhmc_fetch::Sources::load(&paths.sources_toml())?;
    for sid in ["trees", "trees_hpa"] {
        let Some(src) = sources.get(sid) else { continue };
        for f in &src.files {
            let p = paths.raw().join(sid).join(&f.path);
            let mut v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&p)?)?;
            if let Some(fs) = v.get_mut("features").and_then(|f| f.as_array_mut()) {
                fs.retain(|ft| {
                    let mut c = &ft["geometry"]["coordinates"];
                    while c.as_array().and_then(|a| a.first()).is_some_and(|f| f.is_array()) {
                        c = &c[0];
                    }
                    match (c[0].as_f64(), c[1].as_f64()) {
                        (Some(e), Some(n)) => inside(rect, e, n),
                        _ => false,
                    }
                });
            }
            let o = dir.join("data/raw").join(sid).join(&f.path);
            std::fs::create_dir_all(o.parent().expect("dir"))?;
            std::fs::write(&o, serde_json::to_string(&v)? + "\n")?;
        }
    }
    let g = GoldenToml { description: description.into(), tile: [t.rx, t.rz], window };
    std::fs::write(dir.join("golden.toml"), toml::to_string(&g)?)?;
    info!(dir = %dir.display(), "golden tile extracted; run `hhmc golden check --name {name} --update`");
    Ok(())
}

fn copy(src: &Path, dst: &Path) -> Result<()> {
    std::fs::create_dir_all(dst.parent().expect("dir"))?;
    std::fs::copy(src, dst).with_context(|| format!("copying {}", src.display()))?;
    Ok(())
}

fn copy_tree(src: &Path, dst: &Path) -> Result<()> {
    for e in std::fs::read_dir(src)? {
        let e = e?;
        let to = dst.join(e.file_name());
        if e.file_type()?.is_dir() {
            copy_tree(&e.path(), &to)?;
        } else {
            copy(&e.path(), &to)?;
        }
    }
    Ok(())
}

/// Builds and checks golden tiles (all, or `name`); with `update`, rewrites expectations.
pub fn check(paths: &Paths, name: Option<&str>, update: bool) -> Result<()> {
    let gd = golden_dir(paths);
    let mut names: Vec<String> = match name {
        Some(n) => vec![n.to_string()],
        None => std::fs::read_dir(&gd)?
            .filter_map(|e| e.ok())
            .filter(|e| e.path().join("golden.toml").exists())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect(),
    };
    names.sort();
    ensure!(!names.is_empty(), "no golden tiles in {}", gd.display());
    // Clip data is not in the repository: check only the clips cut locally.
    let (have, missing): (Vec<String>, Vec<String>) =
        names.into_iter().partition(|n| gd.join(n).join("data").is_dir());
    if !missing.is_empty() {
        ensure!(
            name.is_none(),
            "golden tile {} has no data: cut it with `hhmc golden extract --name {}`",
            missing[0],
            missing[0]
        );
        warn!(
            skipped = %missing.join(", "),
            "golden tiles without data (cut them from a full build with `hhmc golden extract --name <name>`)"
        );
    }
    let names = have;
    let mut failures = Vec::new();
    for n in &names {
        match check_one(paths, n, update) {
            Ok(()) => info!(golden = %n, "ok"),
            Err(e) => {
                eprintln!("FAIL golden {n}: {e:#}");
                failures.push(n.clone());
            }
        }
    }
    if !failures.is_empty() {
        bail!("golden tiles failed: {}", failures.join(", "));
    }
    Ok(())
}

/// Cuts the data of an existing golden tile again from its `golden.toml`.
pub fn re_extract(paths: &Paths, name: &str) -> Result<()> {
    let g: GoldenToml =
        hhmc_core::config::load_toml(&golden_dir(paths).join(name).join("golden.toml"))?;
    let t = TileKey::new(g.tile[0], g.tile[1]);
    extract(paths, name, t, [g.window[0], g.window[1]], &g.description)
}

fn check_one(paths: &Paths, name: &str, update: bool) -> Result<()> {
    let dir = golden_dir(paths).join(name);
    let g: GoldenToml = hhmc_core::config::load_toml(&dir.join("golden.toml"))?;
    let t = TileKey::new(g.tile[0], g.tile[1]);
    // A clean root: the repository's config, the golden tile's data.
    let root = paths.out().join("golden").join(name);
    if root.exists() {
        std::fs::remove_dir_all(&root)?;
    }
    copy_tree(&paths.config(), &root.join("config"))?;
    copy(&paths.sources_toml(), &root.join("data/sources.toml"))?;
    copy_tree(&dir.join("data"), &root.join("data"))?;
    let gp = Paths { root: root.clone() };
    build_tile(&gp, t)?;
    let qa = crate::qa::qa(&gp, name, &[t], false);
    if let Err(e) = &qa {
        eprintln!("golden {name}: QA: {e:#}");
    }
    // The window's blocks and render.
    let layout = hhmc_world::WorldLayout::new(gp.out().join("world"));
    let tile = hhmc_world::read_tile(&layout.region_file(t), t)?;
    let w = g.window;
    let mut counts: BTreeMap<String, u64> = BTreeMap::new();
    for z in w[1]..w[3] {
        for x in w[0]..w[2] {
            for y in hhmc_core::voxel::MIN_Y..hhmc_core::voxel::MAX_Y_EXCL {
                let s = tile.palette.get(tile.get(x as usize, y, z as usize));
                if &*s.name != "minecraft:air" {
                    *counts.entry(s.name.to_string()).or_default() += 1;
                }
            }
        }
    }
    let top =
        read_png(&gp.out().join("qa/tiles").join(format!("{}_{}", t.rx, t.rz)).join("top.png"))?;
    let (ww, wh) = ((w[2] - w[0]) as usize, (w[3] - w[1]) as usize);
    let mut crop = vec![0u8; ww * wh * 3];
    for z in 0..wh {
        for x in 0..ww {
            let s = ((z + w[1] as usize) * TILE_SIZE as usize + x + w[0] as usize) * 3;
            crop[(z * ww + x) * 3..(z * ww + x) * 3 + 3].copy_from_slice(&top[s..s + 3]);
        }
    }
    let got = Expected { qa_passes: qa.is_ok(), counts };
    let (ej, ep) = (dir.join("expected.json"), dir.join("expected.png"));
    if update {
        std::fs::write(&ej, serde_json::to_string_pretty(&got)? + "\n")?;
        hhmc_qa::ground_qa::write_png(&ep, ww, wh, &crop)?;
        info!(golden = %name, qa_passes = got.qa_passes, "expectations written");
        return Ok(());
    }
    let exp: Expected = serde_json::from_str(&std::fs::read_to_string(&ej)?)?;
    let mut problems = Vec::new();
    if exp.qa_passes != got.qa_passes {
        problems.push(format!("QA passes: expected {}, got {}", exp.qa_passes, got.qa_passes));
    }
    let names: std::collections::BTreeSet<&String> =
        exp.counts.keys().chain(got.counts.keys()).collect();
    for b in names {
        let (a, c) =
            (exp.counts.get(b).copied().unwrap_or(0), got.counts.get(b).copied().unwrap_or(0));
        let allowed = ((a.max(c) as f64 * COUNT_TOLERANCE) as u64).max(MIN_COUNT_DIFF);
        if a.abs_diff(c) > allowed {
            problems.push(format!("{b}: expected {a}, got {c}"));
        }
    }
    let ref_png = read_png(&ep)?;
    ensure!(ref_png.len() == crop.len(), "expected.png has another size");
    let differ = crop
        .chunks(3)
        .zip(ref_png.chunks(3))
        .filter(|(a, b)| a.iter().zip(b.iter()).any(|(x, y)| x.abs_diff(*y) > 24))
        .count();
    if differ as f64 > PIXEL_TOLERANCE * (ww * wh) as f64 {
        problems.push(format!("top-down render: {differ} of {} pixels differ", ww * wh));
        hhmc_qa::ground_qa::write_png(&root.join("got.png"), ww, wh, &crop)?;
    }
    if !problems.is_empty() {
        bail!("{} (rerun with --update if the change is intended)", problems.join("; "));
    }
    Ok(())
}

/// Runs the stages on one tile and writes its region (no template world needed).
fn build_tile(paths: &Paths, t: TileKey) -> Result<()> {
    use hhmc_core::landcfg::{BlocksConfig, BodiesFile, WaterConfig};
    let world = WorldConfig::load(paths)?;
    let blocks = BlocksConfig::load(paths)?;
    let veg = hhmc_core::trees::VegetationConfig::load(paths)?;
    let infra = hhmc_core::infra::InfraConfig::load(paths)?;
    let water = WaterConfig::load(paths)?;
    let bodies = BodiesFile::load(paths)?;
    let cache = hhmc_core::cache::Cache::new(paths.cache());
    let inp = hhmc_ground::GroundInputs {
        paths,
        blocks: &blocks,
        water: &water,
        bodies: &bodies,
        height: &world.height,
    };
    let cx = crate::build::StageCtx {
        paths,
        cache: &cache,
        world: &world,
        blocks: &blocks,
        veg: &veg,
        infra: &infra,
        clip: None,
    };
    let l = crate::build::layers(&cx, &inp, t, true)?;
    let vt = crate::build::compose(&l, &blocks)?;
    let layout = hhmc_world::WorldLayout::new(paths.out().join("world"));
    hhmc_world::write_tile(&vt, &layout.region_dir(), world.minecraft.data_version)?;
    Ok(())
}

fn read_png(p: &Path) -> Result<Vec<u8>> {
    let dec = png::Decoder::new(std::io::BufReader::new(
        std::fs::File::open(p).with_context(|| format!("opening {}", p.display()))?,
    ));
    let mut rd = dec.read_info()?;
    let mut buf = vec![0; rd.output_buffer_size().context("png size")?];
    rd.next_frame(&mut buf)?;
    Ok(buf)
}
