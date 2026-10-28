//! `hhmc`: the single entry point of the pipeline.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};
use hhmc_core::{Areas, Paths, TileKey, WorldConfig, proj, tile};
use tracing::info;

mod build;
mod golden;
mod map;
mod package;
mod qa;
mod report;
mod trailer;
mod world;

#[derive(Parser)]
#[command(name = "hhmc", version, about = "Hamburg 1:1 in Minecraft")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum GoldenCmd {
    /// Cut a 64 × 64 m window of a tile's prepared inputs into tests/golden/<name>/data/.
    /// With only --name, re-cuts an existing clip from its golden.toml.
    Extract {
        #[arg(long)]
        name: String,
        /// The tile, "rx,rz".
        #[arg(long, allow_hyphen_values = true, requires_all = ["at", "description"])]
        tile: Option<String>,
        /// Tile-local block position of the window's north-west corner, "x,z".
        #[arg(long, requires = "tile")]
        at: Option<String>,
        /// What the clip shows.
        #[arg(long, requires = "tile")]
        description: Option<String>,
    },
    /// Build every golden tile (or one) from a clean root, run QA and compare with the
    /// committed expectations.
    Check {
        #[arg(long)]
        name: Option<String>,
        /// Rewrite the expectations instead of comparing.
        #[arg(long)]
        update: bool,
    },
}

#[derive(Subcommand)]
enum Cmd {
    /// Download and verify datasets from data/sources.toml.
    Fetch(FetchArgs),
    /// Normalize fetched data into per-tile rasters and FlatGeobuf (data/prepared/).
    Prep {
        #[arg(long, default_value = "pilot")]
        area: String,
    },
    /// Run prep (for --area) and the per-tile stages; write region files into out/world.
    Build {
        /// Named area from config/areas.toml (`--bbox` is accepted as an alias).
        #[arg(long, alias = "bbox")]
        area: Option<String>,
        /// Explicit tiles "rx,rz;rx,rz;…" instead of an area.
        #[arg(long)]
        tiles: Option<String>,
        /// Ignore caches.
        #[arg(long)]
        force: bool,
    },
    /// Run the QA checks on out/world and write out/qa/.
    Qa {
        #[arg(long)]
        area: Option<String>,
        #[arg(long)]
        tiles: Option<String>,
        /// Record the last forced build's tile times as the performance baseline
        /// (config/perf_baseline.toml).
        #[arg(long)]
        set_baseline: bool,
    },
    /// Render the QA viewpoints of config/qa.toml from out/world into out/qa/views/.
    Views {
        /// Image width in pixels.
        #[arg(long, default_value_t = 1280)]
        width: usize,
        /// Read `[[viewpoint]]`s from this TOML file instead of config/qa.toml.
        #[arg(long)]
        config: Option<PathBuf>,
        /// Output directory (default out/qa/views).
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Derive config/block_colors.toml from the pinned client jar.
    Palette,
    /// Write the top-down web map of the built tiles into out/map/.
    Map {
        #[arg(long)]
        area: Option<String>,
        #[arg(long)]
        tiles: Option<String>,
    },
    /// Golden tiles: small clips of real inputs in tests/golden/, cut from a full build.
    #[command(subcommand)]
    Golden(GoldenCmd),
    /// Generate ATTRIBUTION.md from data/sources.toml.
    Attribution {
        /// Fail if the file is out of date instead of writing it.
        #[arg(long)]
        check: bool,
    },
    /// List the members of a remote zip (reads only its central directory): compressed
    /// size, size, CRC-32 and name.
    ZipLs {
        url: String,
        /// Only members containing this substring.
        #[arg(long)]
        filter: Option<String>,
    },
    /// World assembly: template world, test world, write benchmark.
    #[command(subcommand)]
    World(WorldCmd),
    /// Check a world on a headless vanilla server. Without arguments: build and check the
    /// test world. With --area: check out/world, asserting ground and water blocks
    /// sampled from the ground models.
    ServerTest {
        /// Existing world directory to test instead of the generated test world.
        #[arg(long)]
        world: Option<PathBuf>,
        /// Tiles of that world to load.
        #[arg(long)]
        tiles: Option<String>,
        /// Check out/world for this area.
        #[arg(long)]
        area: Option<String>,
    },
    /// Manage named areas in config/areas.toml.
    #[command(subcommand)]
    Area(AreaCmd),
    /// Zip the built world of an area with ATTRIBUTION.md and a README for players
    /// (out/package/<folder>-<area>.zip, or .partNofM.zip with --part-size).
    Package {
        #[arg(long, default_value = "full")]
        area: String,
        /// Name of the world folder inside the zip.
        #[arg(long, default_value = "hamburg-1to1")]
        folder: String,
        /// Split into zips of at most this many bytes, unzipped into the same folder
        /// (e.g. 2000000000 for GitHub's 2 GiB limit per release file). Default: one zip.
        #[arg(long)]
        part_size: Option<u64>,
    },
    /// Trailer camera tour from config/trailer.toml: tour datapack, Flashback editor state
    /// and preview viewpoints in out/trailer/.
    Trailer {
        /// Install the editor state for this Flashback replay (its name in
        /// flashback/replays/, without .zip) instead of generating.
        #[arg(long, requires = "minecraft")]
        install: Option<String>,
        /// The game directory holding flashback/ (e.g. a Prism instance's minecraft/).
        #[arg(long)]
        minecraft: Option<PathBuf>,
        /// With --install: ticks between starting the recording and starting the tour
        /// (default: the tick of the tour's first chat message in the replay).
        #[arg(long)]
        offset: Option<i64>,
    },
    /// Manage the file lists in data/sources.toml.
    #[command(subcommand)]
    Sources(SourcesCmd),
}

#[derive(Subcommand)]
enum SourcesCmd {
    /// List the files an area needs from every source with a `tiled` rule into
    /// data/sources/<id>.toml (reads zip central directories only).
    Expand {
        #[arg(long)]
        area: String,
        /// Only these sources (comma-separated ids).
        #[arg(long, value_delimiter = ',')]
        datasets: Vec<String>,
    },
}

#[derive(Args)]
struct FetchArgs {
    /// Only fetch files intersecting this area (plus the tile read buffer).
    #[arg(long)]
    area: Option<String>,
    /// Only this dataset id; repeatable.
    #[arg(long = "dataset")]
    datasets: Vec<String>,
    /// Record sha256 and size of files that have none yet.
    #[arg(long)]
    pin: bool,
    /// Accept changed upstream content and record its new checksum.
    #[arg(long)]
    repin: bool,
    /// Allow single files over 5 GB.
    #[arg(long)]
    allow_large: bool,
    /// List what would be downloaded.
    #[arg(long)]
    dry_run: bool,
}

#[derive(Subcommand)]
enum WorldCmd {
    /// Let the vanilla server create the void template world (level.dat, saved data).
    Template {
        #[arg(long)]
        force: bool,
    },
    /// Write the test world to out/test-world.
    TestWorld,
    /// Time writing one dense region.
    Bench,
    /// Describe the building meshes covering an EPSG:25832 point.
    BuildingAt { e: f64, n: f64 },
    /// Print the NBT structure of one chunk of a region file.
    DumpChunk { region: PathBuf, cx: usize, cz: usize },
}

#[derive(Subcommand)]
enum AreaCmd {
    /// Define an area from a WGS84 box: transform with PROJ, snap outward to tiles.
    FromWgs84 {
        /// Area name, e.g. `pilot`.
        name: String,
        /// South-west corner `lat,lon`.
        #[arg(long, value_parser = parse_latlon)]
        sw: [f64; 2],
        /// North-east corner `lat,lon`.
        #[arg(long, value_parser = parse_latlon)]
        ne: [f64; 2],
        #[arg(long)]
        description: String,
    },
    /// Define an area from a city boundary (GeoJSON, EPSG:25832): every tile with at
    /// least one column center inside; the build clips to the boundary.
    FromBoundary {
        /// Area name, e.g. `full`.
        name: String,
        /// Boundary GeoJSON relative to the repository root.
        #[arg(long, default_value = "data/raw/boundary/landesgrenze.geojson")]
        boundary: String,
        /// Leave out polygon parts smaller than this (islands).
        #[arg(long, default_value_t = 10.0)]
        min_part_km2: f64,
        #[arg(long)]
        description: String,
    },
    /// Print an area's tiles and boxes.
    Show { name: String },
}

fn parse_latlon(s: &str) -> Result<[f64; 2], String> {
    let (a, b) = s.split_once(',').ok_or("expected lat,lon")?;
    let p = |v: &str| v.trim().parse::<f64>().map_err(|e| e.to_string());
    Ok([p(a)?, p(b)?])
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();
    let cli = Cli::parse();
    let paths = Paths::discover()?;
    match cli.cmd {
        Cmd::Fetch(a) => fetch(&paths, a),
        Cmd::Qa { area, tiles, set_baseline } => {
            let t = build::tiles_arg(&paths, area.as_deref(), tiles.as_deref())?;
            let name = area.clone().or(tiles.clone()).unwrap_or_default();
            qa::qa(&paths, &name, &t, set_baseline)
        }
        Cmd::Views { width, config, out } => qa::views(&paths, width, config, out),
        Cmd::Golden(GoldenCmd::Extract { name, tile, at, description }) => {
            match (tile, at, description) {
                (Some(tile), Some(at), Some(description)) => {
                    let t = tile::parse_tile_list(&tile).map_err(anyhow::Error::msg)?;
                    let (x, z) = at.split_once(',').context("--at x,z")?;
                    golden::extract(
                        &paths,
                        &name,
                        t[0],
                        [x.trim().parse()?, z.trim().parse()?],
                        &description,
                    )
                }
                _ => golden::re_extract(&paths, &name),
            }
        }
        Cmd::Golden(GoldenCmd::Check { name, update }) => {
            golden::check(&paths, name.as_deref(), update)
        }
        Cmd::Map { area, tiles } => {
            let t = build::tiles_arg(&paths, area.as_deref(), tiles.as_deref())?;
            map::map(&paths, &t)
        }
        Cmd::Palette => {
            let cfg = WorldConfig::load(&paths)?;
            let jar = hhmc_qa::mojang::ensure_client_jar(&paths.root, &cfg.minecraft.version)?;
            let colors = hhmc_qa::palette::compute(&jar)?;
            let out = paths.config().join("block_colors.toml");
            hhmc_qa::palette::write_toml(&out, &cfg.minecraft.version, &colors)?;
            info!(blocks = colors.len(), path = %out.display(), "palette written");
            Ok(())
        }
        Cmd::Prep { area } => hhmc_prep::run(&paths, &area),
        Cmd::Build { area, tiles, force } => {
            if let Some(a) = &area {
                hhmc_prep::run(&paths, a)?;
            }
            let t = build::tiles_arg(&paths, area.as_deref(), tiles.as_deref())?;
            // An area's boundary clips the build (also of a subset given with --tiles).
            let clip = build::Clip::for_area(&paths, area.as_deref())?;
            build::build(&paths, &t, clip.as_ref(), force)
        }
        Cmd::Attribution { check } => attribution(&paths, check),
        Cmd::World(WorldCmd::Template { force }) => world::template(&paths, force).map(|_| ()),
        Cmd::World(WorldCmd::TestWorld) => world::test_world(&paths).map(|_| ()),
        Cmd::World(WorldCmd::Bench) => world::bench(),
        Cmd::World(WorldCmd::BuildingAt { e, n }) => world::building_at(&paths, e, n),
        Cmd::World(WorldCmd::DumpChunk { region, cx, cz }) => world::dump_chunk(&region, cx, cz),
        Cmd::ServerTest { world, tiles, area } => world::server_test(&paths, world, tiles, area),
        Cmd::Area(AreaCmd::FromWgs84 { name, sw, ne, description }) => {
            area_from_wgs84(&paths, &name, sw, ne, &description)
        }
        Cmd::ZipLs { url, filter } => {
            for m in hhmc_fetch::remote_zip::list_remote_zip(&url)? {
                if filter.as_deref().is_none_or(|f| m.name.contains(f)) {
                    println!("{}\t{}\t{:08x}\t{}", m.compressed_size, m.size, m.crc32, m.name);
                }
            }
            Ok(())
        }
        Cmd::Area(AreaCmd::FromBoundary { name, boundary, min_part_km2, description }) => {
            area_from_boundary(&paths, &name, &boundary, min_part_km2, &description)
        }
        Cmd::Area(AreaCmd::Show { name }) => area_show(&paths, &name),
        Cmd::Trailer { install: Some(replay), minecraft: Some(dir), offset } => {
            trailer::install(&paths, &dir, &replay, offset)
        }
        Cmd::Trailer { .. } => trailer::trailer(&paths).map(|_| ()),
        Cmd::Package { area, folder, part_size } => {
            package::package(&paths, &area, &folder, part_size).map(|_| ())
        }
        Cmd::Sources(SourcesCmd::Expand { area, datasets }) => {
            let world = WorldConfig::load(&paths)?;
            let areas = Areas::load(&paths)?;
            let boxes: Vec<_> =
                areas.get(&area)?.tiles.iter().map(|t| t.read_bbox(&world.origin)).collect();
            for (id, n) in hhmc_fetch::expand(&paths.sources_toml(), &boxes, &datasets)? {
                println!("{id}: {n} files");
            }
            Ok(())
        }
    }
}

fn fetch(paths: &Paths, a: FetchArgs) -> Result<()> {
    let bbox = match &a.area {
        Some(name) => {
            let world = WorldConfig::load(paths)?;
            let areas = Areas::load(paths)?;
            Some(areas.get(name)?.read_bbox(&world.origin).context("area has no tiles")?)
        }
        None => None,
    };
    let opts = hhmc_fetch::FetchOptions {
        datasets: a.datasets,
        bbox,
        pin: a.pin,
        repin: a.repin,
        allow_large: a.allow_large,
        dry_run: a.dry_run,
    };
    let r = hhmc_fetch::fetch(&paths.sources_toml(), &paths.raw(), &opts)?;
    info!(
        verified = r.verified,
        downloaded = r.downloaded,
        pinned = r.pinned,
        outside_area = r.skipped_area,
        mb = r.bytes_downloaded >> 20,
        "fetch done"
    );
    if r.pinned > 0 {
        attribution(paths, false)?;
    }
    Ok(())
}

fn attribution(paths: &Paths, check: bool) -> Result<()> {
    let sources = hhmc_fetch::Sources::load(&paths.sources_toml())?;
    let text = hhmc_fetch::render_attribution(&sources);
    let path: PathBuf = paths.root.join("ATTRIBUTION.md");
    if check {
        let current = std::fs::read_to_string(&path).unwrap_or_default();
        if current != text {
            bail!("ATTRIBUTION.md is out of date; run `hhmc attribution`");
        }
    } else {
        std::fs::write(&path, text)?;
        info!("wrote {}", path.display());
    }
    Ok(())
}

fn area_from_wgs84(
    paths: &Paths,
    name: &str,
    sw: [f64; 2],
    ne: [f64; 2],
    description: &str,
) -> Result<()> {
    let world = WorldConfig::load(paths)?;
    let bbox = proj::wgs84_box_to_25832(sw, ne)?;
    let tiles = tile::tiles_covering(&world.origin, &bbox);
    info!(name, ?bbox, count = tiles.len(), "computed tiles");

    let path = paths.config().join("areas.toml");
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    let mut doc: toml_edit::DocumentMut = text.parse()?;
    let mut t = toml_edit::Table::new();
    t["description"] = toml_edit::value(description);
    t["wgs84_sw"] = toml_edit::value(float_array(&sw, 4));
    t["wgs84_ne"] = toml_edit::value(float_array(&ne, 4));
    t["bbox_25832"] =
        toml_edit::value(float_array(&[bbox.min_e, bbox.min_n, bbox.max_e, bbox.max_n], 1));
    let mut arr = toml_edit::Array::new();
    for k in &tiles {
        let mut pair = toml_edit::Array::new();
        pair.push(k.rx as i64);
        pair.push(k.rz as i64);
        pair.decor_mut().set_prefix("\n  ");
        arr.push(pair);
    }
    arr.set_trailing("\n");
    arr.set_trailing_comma(true);
    t["tiles"] = toml_edit::value(arr);
    t.decor_mut().set_prefix(format!(
        "# Named tile set; tile = 512 m = one region file r.<rx>.<rz>.mca.\n# Generated by `hhmc area from-wgs84 {name}`: the WGS84 box is transformed to \
         EPSG:25832 with PROJ (edges densified) and snapped outward to whole 512 m tiles.\n"
    ));
    doc[name] = toml_edit::Item::Table(t);
    std::fs::write(&path, doc.to_string())?;
    info!("wrote {}", path.display());
    Ok(())
}

fn area_from_boundary(
    paths: &Paths,
    name: &str,
    boundary: &str,
    min_part_km2: f64,
    description: &str,
) -> Result<()> {
    let world = WorldConfig::load(paths)?;
    let b = hhmc_core::boundary::Boundary::load(&paths.root.join(boundary), min_part_km2)?;
    let bbox = b.bbox();
    // Tiles of the grid covering the boundary box that hold at least one inside column.
    let grid = tile::tiles_covering(&world.origin, &bbox);
    let all = grid.iter().map(|t| t.bbox(&world.origin)).reduce(|a, c| a.union(&c)).unwrap();
    let rx0 = grid[0].rx;
    let cols = (grid.last().unwrap().rx - rx0 + 1) as usize;
    let mut hit = vec![false; grid.len()];
    let ts = tile::TILE_SIZE as usize;
    b.for_each_inside(&all, |c, r| hit[(r / ts) * cols + c / ts] = true);
    let tiles: Vec<TileKey> = grid.iter().zip(&hit).filter(|(_, h)| **h).map(|(t, _)| *t).collect();
    info!(
        name,
        parts = b.polygons.len(),
        km2 = b.area_km2(),
        count = tiles.len(),
        "computed tiles"
    );

    let path = paths.config().join("areas.toml");
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    let mut doc: toml_edit::DocumentMut = text.parse()?;
    let mut t = toml_edit::Table::new();
    t["description"] = toml_edit::value(description);
    t["bbox_25832"] =
        toml_edit::value(float_array(&[bbox.min_e, bbox.min_n, bbox.max_e, bbox.max_n], 1));
    let mut bt = toml_edit::InlineTable::new();
    bt.insert("path", boundary.into());
    bt.insert("min_part_km2", min_part_km2.into());
    t["boundary"] = toml_edit::value(bt);
    let mut arr = toml_edit::Array::new();
    for k in &tiles {
        let mut pair = toml_edit::Array::new();
        pair.push(k.rx as i64);
        pair.push(k.rz as i64);
        pair.decor_mut().set_prefix("\n  ");
        arr.push(pair);
    }
    arr.set_trailing("\n");
    arr.set_trailing_comma(true);
    t["tiles"] = toml_edit::value(arr);
    t.decor_mut().set_prefix(format!(
        "\n# Generated by `hhmc area from-boundary {name}`: every 512 m tile with a column \
         center inside the\n# boundary's parts of at least {min_part_km2} km²; the build \
         leaves columns outside it empty.\n"
    ));
    doc[name] = toml_edit::Item::Table(t);
    std::fs::write(&path, doc.to_string())?;
    info!("wrote {}", path.display());
    Ok(())
}

fn float_array(v: &[f64], decimals: usize) -> toml_edit::Array {
    let mut a = toml_edit::Array::new();
    for x in v {
        let r = (x * 10f64.powi(decimals as i32)).round() / 10f64.powi(decimals as i32);
        a.push(r);
    }
    a
}

fn area_show(paths: &Paths, name: &str) -> Result<()> {
    let world = WorldConfig::load(paths)?;
    let areas = Areas::load(paths)?;
    let area = areas.get(name)?;
    println!("{name}: {}", area.description);
    println!("tiles: {}", area.tiles.len());
    if let Some(b) = area.tiles_bbox(&world.origin) {
        println!("tiles bbox (EPSG:25832): {b:?}");
    }
    if let Some(b) = area.read_bbox(&world.origin) {
        println!("read bbox incl. buffer:  {b:?}");
    }
    let list: Vec<String> = area.tiles.iter().map(|t| t.to_string()).collect();
    println!("--tiles \"{}\"", list.join(";"));
    Ok(())
}
