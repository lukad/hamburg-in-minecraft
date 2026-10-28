//! `hhmc world …` and `hhmc server-test`.

use std::path::PathBuf;
use std::time::Instant;

use anyhow::{Context, Result, bail};
use hhmc_core::voxel::MIN_Y;
use hhmc_core::{BlockState, Paths, TileKey, VoxelTile, WorldConfig, tile};
use hhmc_qa::harness::{self, BlockAssert};
use hhmc_qa::server::{self, ServerSetup};
use hhmc_world::WorldLayout;
use hhmc_world::level::{RuleValue, assemble_world};
use hhmc_world::testworld::test_tile;
use tracing::info;

const TEST_TILES: [TileKey; 4] =
    [TileKey::new(-1, -1), TileKey::new(0, -1), TileKey::new(-1, 0), TileKey::new(0, 0)];

fn setup(paths: &Paths, cfg: &WorldConfig) -> Result<ServerSetup> {
    let (jar, java_major) =
        hhmc_qa::mojang::ensure_server_jar(&paths.root, &cfg.minecraft.version)?;
    if java_major != cfg.minecraft.java_major {
        bail!(
            "config/world.toml says Java {} but Mojang's version JSON says {java_major}",
            cfg.minecraft.java_major
        );
    }
    Ok(ServerSetup {
        jar,
        work_dir: paths.out().join("server"),
        java: std::env::var("HHMC_JAVA").unwrap_or_else(|_| "java".to_owned()),
        java_major,
        max_heap: "4G".to_owned(),
    })
}

/// Returns the template world directory, creating it with the server if needed.
pub fn template(paths: &Paths, force: bool) -> Result<PathBuf> {
    let cfg = WorldConfig::load(paths)?;
    let dir = paths.out().join("template").join(&cfg.minecraft.version);
    if dir.join("level.dat").is_file() && !force {
        return Ok(dir);
    }
    let s = setup(paths, &cfg)?;
    server::create_template(&s, &dir)?;
    info!(dir = %dir.display(), "template world created");
    Ok(dir)
}

pub fn game_rules(cfg: &WorldConfig) -> Result<Vec<(String, RuleValue)>> {
    cfg.game_rules
        .iter()
        .map(|(k, v)| match v {
            toml::Value::Boolean(b) => Ok((k.clone(), RuleValue::Bool(*b))),
            toml::Value::Integer(i) => Ok((k.clone(), RuleValue::Int(i32::try_from(*i)?))),
            _ => bail!("game rule {k}: only booleans and integers are supported"),
        })
        .collect()
}

pub fn test_world(paths: &Paths) -> Result<(PathBuf, Vec<BlockAssert>)> {
    let cfg = WorldConfig::load(paths)?;
    let tpl = template(paths, false)?;
    let layout = WorldLayout::new(paths.out().join("test-world"));
    assemble_world(&tpl, &layout, [0, 2, 0], "hhmc test world", &game_rules(&cfg)?)?;
    let mut asserts = Vec::new();
    for key in TEST_TILES {
        let (t, exp) = test_tile(key);
        let start = Instant::now();
        hhmc_world::write_tile(&t, &layout.region_dir(), cfg.minecraft.data_version)?;
        info!(tile = %key, ms = start.elapsed().as_millis(), "wrote region");
        asserts.extend(exp.into_iter().map(|e| BlockAssert {
            x: e.x,
            y: e.y,
            z: e.z,
            state: e.state,
        }));
    }
    info!(world = %layout.root.display(), asserts = asserts.len(), "test world written");
    Ok((layout.root, asserts))
}

pub fn server_test(
    paths: &Paths,
    world: Option<PathBuf>,
    tiles: Option<String>,
    area: Option<String>,
) -> Result<()> {
    let cfg = WorldConfig::load(paths)?;
    let (src, asserts, tiles) = match (world, area) {
        (None, Some(area)) => {
            let t = crate::build::tiles_arg(paths, Some(&area), tiles.as_deref())?;
            let a = ground_asserts(paths, &t)?;
            (paths.out().join("world"), a, t)
        }
        (None, None) => {
            let (w, a) = test_world(paths)?;
            (w, a, TEST_TILES.to_vec())
        }
        (Some(w), _) => {
            let t = tile::parse_tile_list(
                tiles.as_deref().context("--tiles is required with --world")?,
            )
            .map_err(anyhow::Error::msg)?;
            (w, Vec::new(), t)
        }
    };
    // Run on a copy: the server rewrites the chunks it loads (light, heightmaps).
    let universe = paths.out().join("server/run");
    let name = src.file_name().and_then(|n| n.to_str()).context("world dir name")?.to_owned();
    let copy = universe.join(&name);
    let _ = std::fs::remove_dir_all(&copy);
    // Only the tested tiles' region files (the full city is 13 GB); the server generates
    // anything else from the void template.
    let wanted: std::collections::HashSet<String> =
        tiles.iter().map(|t| t.region_file_name()).collect();
    hhmc_world::layout::copy_dir_filtered(&src, &copy, &|p| {
        p.extension().is_none_or(|x| x != "mca")
            || p.file_name().and_then(|n| n.to_str()).is_some_and(|n| wanted.contains(n))
    })?;
    let s = setup(paths, &cfg)?;
    let report = harness::run(&s, &universe, &name, &tiles, &asserts)?;
    // The QA report shows the last server test of the built world.
    if name == "world" {
        #[derive(serde::Serialize)]
        struct ServerResult<'a> {
            tiles: Vec<[i32; 2]>,
            unix_time: u64,
            passed: bool,
            report: &'a harness::HarnessReport,
        }
        let r = ServerResult {
            tiles: tiles.iter().map(|t| [t.rx, t.rz]).collect(),
            unix_time: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs()),
            passed: report.passed() && !(report.asserts == 0 && !asserts.is_empty()),
            report: &report,
        };
        std::fs::create_dir_all(paths.out().join("qa"))?;
        std::fs::write(
            paths.out().join("qa").join("server.json"),
            serde_json::to_string_pretty(&r)? + "\n",
        )?;
    }
    info!(
        asserts = report.asserts,
        failures = report.failures.len(),
        log_problems = report.log_problems.len(),
        "server test done"
    );
    for f in report.failures.iter().take(20) {
        eprintln!("FAIL {f}");
    }
    for p in report.log_problems.iter().take(20) {
        eprintln!("LOG  {p}");
    }
    if !report.passed() {
        bail!("server test failed");
    }
    if report.asserts == 0 && !asserts.is_empty() {
        bail!("no assertions were checked");
    }
    println!(
        "server test passed: {} tiles, {} chunks loaded, {} block assertions",
        tiles.len(),
        report.chunks_loaded,
        report.asserts
    );
    Ok(())
}

/// Writes a dense region (terrain columns everywhere, varied surface, some buildings) and
/// reports the time; the target is under 2 s.
pub fn bench() -> Result<()> {
    let key = TileKey::new(0, 0);
    let mut t = VoxelTile::new(key, "minecraft:plains");
    let ids: Vec<_> =
        ["bedrock", "stone", "dirt", "grass_block", "water", "bricks", "glass", "oak_planks"]
            .iter()
            .map(|n| t.intern(&BlockState::new(n)))
            .collect();
    let stairs = t.intern(
        &BlockState::parse(
            "brick_stairs[facing=north,half=bottom,shape=straight,waterlogged=false]",
        )
        .map_err(anyhow::Error::msg)?,
    );
    let start = Instant::now();
    for z in 0..512usize {
        for x in 0..512usize {
            let h = ((x as f64 / 37.0).sin() * 6.0 + (z as f64 / 53.0).cos() * 5.0) as i32 + 4;
            t.set(x, MIN_Y, z, ids[0]);
            t.fill_column(x, z, MIN_Y + 1, h - 4, ids[1]);
            t.fill_column(x, z, h - 3, h - 1, ids[2]);
            t.set(x, h, z, if h < 2 { ids[4] } else { ids[3] });
            if (x / 24 + z / 24) % 3 == 0 && x % 24 > 3 && z % 24 > 3 {
                let top = h + 10 + ((x / 24 * 7 + z / 24 * 3) % 30) as i32;
                let wall = x % 24 == 4 || x % 24 == 23 || z % 24 == 4 || z % 24 == 23;
                for y in h + 1..=top {
                    if wall || (y - h) % 3 == 0 {
                        t.set(x, y, z, if wall && y % 3 == 1 { ids[6] } else { ids[5] });
                    }
                }
                t.set(x, top + 1, z, if x % 2 == 0 { stairs } else { ids[7] });
            }
        }
    }
    let fill = start.elapsed();
    let start = Instant::now();
    let bytes = hhmc_world::tile_to_region(&t, 5023)?;
    let write = start.elapsed();
    let dir = std::env::temp_dir().join("hhmc-bench");
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join(key.region_file_name()), &bytes)?;
    println!(
        "dense region: fill {:.2} s, encode {:.2} s, {:.1} MB",
        fill.as_secs_f64(),
        write.as_secs_f64(),
        bytes.len() as f64 / 1e6
    );
    Ok(())
}

/// Prints the top-level keys of one chunk and the keys of its first non-empty section.
pub fn dump_chunk(region: &std::path::Path, cx: usize, cz: usize) -> Result<()> {
    let bytes = std::fs::read(region)?;
    let chunks = hhmc_world::region::disassemble(&bytes)?;
    let nbt = chunks[cz * 32 + cx].as_ref().context("chunk not present")?;
    let v: fastnbt::Value = fastnbt::from_bytes(nbt)?;
    let fastnbt::Value::Compound(root) = v else { bail!("not a compound") };
    let mut keys: Vec<_> = root.keys().cloned().collect();
    keys.sort();
    for k in keys {
        let short = match &root[&k] {
            fastnbt::Value::List(l) => format!("list[{}]", l.len()),
            fastnbt::Value::Compound(c) => {
                let mut ks: Vec<_> = c.keys().cloned().collect();
                ks.sort();
                format!("{{{}}}", ks.join(", "))
            }
            other => format!("{other:?}"),
        };
        println!("{k}: {short}");
    }
    if let Some(fastnbt::Value::List(secs)) = root.get("sections") {
        for s in secs {
            if let fastnbt::Value::Compound(c) = s {
                let mut ks: Vec<_> = c.keys().cloned().collect();
                ks.sort();
                println!("section Y={:?}: {}", c.get("Y"), ks.join(", "));
                if let Some(fastnbt::Value::Compound(bs)) = c.get("block_states")
                    && let Some(p) = bs.get("palette")
                {
                    println!("  palette: {p:?}");
                }
            }
        }
    }
    Ok(())
}

/// The topmost block and the air above it on a 16 m grid of every tile, read from the
/// region files we wrote, so the server is checked against exactly what the build produced
/// (terrain, water, building roofs with stair facings and slab types).
fn ground_asserts(paths: &Paths, tiles: &[TileKey]) -> Result<Vec<BlockAssert>> {
    let layout = WorldLayout::new(paths.out().join("world"));
    let mut out = Vec::new();
    for &t in tiles {
        let vt = hhmc_world::read_tile(&layout.region_file(t), t)?;
        let (bx, bz) = t.min_block();
        for z in (5..512).step_by(16) {
            for x in (7..512).step_by(16) {
                let Some(y) = (MIN_Y..320).rev().find(|&y| vt.get(x, y, z) != hhmc_core::AIR)
                else {
                    continue;
                };
                let (wx, wz) = (bx + x as i32, bz + z as i32);
                out.push(BlockAssert {
                    x: wx,
                    y,
                    z: wz,
                    state: vt.palette.get(vt.get(x, y, z)).clone(),
                });
                if y < 319 {
                    out.push(BlockAssert { x: wx, y: y + 1, z: wz, state: BlockState::air() });
                }
            }
        }
    }
    Ok(out)
}

/// Describes the building meshes whose voxel columns cover the EPSG:25832 point (e, n).
pub fn building_at(paths: &Paths, e: f64, n: f64) -> Result<()> {
    let cfg = WorldConfig::load(paths)?;
    let (x, z) = cfg.origin.block_at(e, n);
    let t = TileKey::from_block(x, z);
    let bin = paths.prepared().join("buildings").join(format!("{}_{}.bin", t.rx, t.rz));
    for m in hhmc_buildings::load_meshes(&bin)? {
        let v = hhmc_buildings::voxelize(&m, &cfg.origin);
        let Some(iv) = v.columns.get(&(x, z)) else { continue };
        let mut kinds: std::collections::BTreeMap<String, usize> =
            std::collections::BTreeMap::new();
        for tri in m.parts.iter().flatten() {
            *kinds.entry(format!("{:?}", tri.kind)).or_default() += 1;
        }
        let mut extra: std::collections::BTreeMap<String, usize> =
            std::collections::BTreeMap::new();
        for tri in &m.extras {
            *extra.entry(format!("{:?}", tri.kind)).or_default() += 1;
        }
        println!(
            "{} source={} parts={} tris={:?} extras={:?} bbox h {:.1}..{:.1} columns={} odd={}",
            m.id,
            m.source,
            m.parts.len(),
            kinds,
            extra,
            m.bbox[2],
            m.bbox[5],
            v.columns.len(),
            v.odd_columns
        );
        let ivs: Vec<String> = iv.iter().map(|i| format!("{}..{}", i.bot, i.top)).collect();
        println!("  column ({x},{z}) intervals {}", ivs.join(" "));
        for (pi, part) in m.parts.iter().enumerate() {
            let zs = part.iter().flat_map(|t| t.v.iter().map(|v| v[2]));
            let (lo, hi) =
                zs.fold((f64::INFINITY, f64::NEG_INFINITY), |a, z| (a.0.min(z), a.1.max(z)));
            println!("  part {pi}: {} tris, h {lo:.1}..{hi:.1}", part.len());
        }
    }
    Ok(())
}
