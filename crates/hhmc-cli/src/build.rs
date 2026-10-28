//! `hhmc build`: per-tile stages, compose and region writing into out/world.

use std::time::Instant;

use anyhow::{Context, Result};
use hhmc_core::boundary::Boundary;
use hhmc_core::cache::{Cache, KeyBuilder};
use hhmc_core::ground::{GroundModel, NO_WATER};
use hhmc_core::infra::{InfraConfig, TileInfra};
use hhmc_core::landcfg::{BlocksConfig, BodiesFile, WaterConfig};
use hhmc_core::trees::{TileTrees, VegetationConfig};
use hhmc_core::voxel::{MAX_Y_EXCL, MIN_Y};
use hhmc_core::{Areas, Paths, TILE_SIZE, TileKey, WorldConfig, tile};
use hhmc_ground::{GroundInputs, cache_key, ground};
use hhmc_world::WorldLayout;
use hhmc_world::compose::{COMPOSE_VERSION, compose_ground};
use hhmc_world::level::{assemble_world, patch_level_dat};
use rayon::prelude::*;
use tracing::info;

pub fn tiles_arg(paths: &Paths, area: Option<&str>, tiles: Option<&str>) -> Result<Vec<TileKey>> {
    match (area, tiles) {
        (_, Some(t)) => tile::parse_tile_list(t).map_err(anyhow::Error::msg),
        (Some(a), None) => Ok(Areas::load(paths)?.get(a)?.tiles.clone()),
        (None, None) => anyhow::bail!("give --area or --tiles"),
    }
}

/// Ground model of a tile, from the cache when its key is unchanged.
pub fn ground_model(
    paths: &Paths,
    cache: &Cache,
    inp: &GroundInputs,
    t: TileKey,
    force: bool,
) -> Result<(GroundModel, [u8; 32])> {
    let key = cache_key(paths, t)?;
    if !force && let Some(m) = cache.get::<GroundModel>("ground", t, &key) {
        return Ok((m, key));
    }
    let m = ground(inp, t).with_context(|| format!("ground stage, tile {t}"))?;
    cache.put("ground", t, &key, &m)?;
    Ok((m, key))
}

/// What every per-tile stage needs.
pub struct StageCtx<'a> {
    pub paths: &'a Paths,
    pub cache: &'a Cache,
    pub world: &'a WorldConfig,
    pub blocks: &'a BlocksConfig,
    pub veg: &'a VegetationConfig,
    pub infra: &'a InfraConfig,
    /// The boundary the area is clipped to.
    pub clip: Option<&'a Clip>,
}

/// An area's boundary: columns outside it are left empty.
pub struct Clip {
    pub boundary: Boundary,
    /// Hash of the boundary file and the minimum part size.
    pub key: [u8; 32],
}

impl Clip {
    /// The clip of a named area, if it has a boundary.
    pub fn for_area(paths: &Paths, area: Option<&str>) -> Result<Option<Clip>> {
        let Some(name) = area else { return Ok(None) };
        let areas = Areas::load(paths)?;
        let a = areas.get(name)?;
        let (Some(spec), Some(boundary)) = (&a.boundary, a.boundary(paths)?) else {
            return Ok(None);
        };
        let key = KeyBuilder::new("clip", 1)
            .file(&paths.root.join(&spec.path))?
            .bytes(&spec.min_part_km2.to_le_bytes())
            .finish();
        Ok(Some(Clip { boundary, key }))
    }

    /// Inside flags of a tile's columns (`z * 512 + x`); `None` when all are inside.
    pub fn mask(&self, world: &WorldConfig, t: TileKey) -> Option<Vec<bool>> {
        let b = t.bbox(&world.origin);
        let n = TILE_SIZE as usize;
        let m = self.boundary.mask(b.min_e, b.max_n, n, n);
        (!m.iter().all(|&v| v)).then_some(m)
    }
}

/// Infra layer of a tile, from the cache when its key is unchanged.
pub fn infra_layer(
    cx: &StageCtx,
    g: &GroundModel,
    gkey: &[u8; 32],
    t: TileKey,
    force: bool,
) -> Result<(hhmc_infra::InfraLayer, [u8; 32])> {
    let (paths, cache) = (cx.paths, cx.cache);
    let list = TileInfra::path(paths, t);
    let bdom = paths.prepared().join("bdom1").join(format!("{}_{}.tif", t.rx, t.rz));
    let alkis = paths.prepared().join("landuse_alkis").join(format!("{}_{}.tif", t.rx, t.rz));
    let dgm = paths.prepared().join("dgm1").join(format!("{}_{}.tif", t.rx, t.rz));
    let key = KeyBuilder::new("infra", hhmc_infra::INFRA_VERSION)
        .bytes(gkey)
        .file(&list)?
        .file(&bdom)?
        .file(&dgm)?
        .file(&alkis)?
        .file(&InfraConfig::path(paths))?
        .file(&paths.config().join("blocks.toml"))?
        .file(&paths.config().join("world.toml"))?
        .finish();
    if !force && let Some(l) = cache.get("infra", t, &key) {
        return Ok((l, key));
    }
    let structures = TileInfra::load(&list).with_context(|| format!("structures of tile {t}"))?;
    let l = hhmc_infra::infra(&hhmc_infra::InfraInputs {
        structures: &structures,
        cfg: cx.infra,
        blocks: cx.blocks,
        origin: &cx.world.origin,
        height: &cx.world.height,
        ground: g,
        bdom: &hhmc_core::raster::read_f32(&bdom)?,
        dgm: Some(&hhmc_core::raster::read_f32(&dgm)?),
        alkis_class: Some(&hhmc_core::raster::read_u8(&alkis)?),
    })?;
    cache.put("infra", t, &key, &l)?;
    Ok((l, key))
}

/// Landmark layer of a tile (config/landmarks.toml), from the cache when its key is
/// unchanged.
pub fn landmark_layer(
    cx: &StageCtx,
    g: &GroundModel,
    gkey: &[u8; 32],
    t: TileKey,
    force: bool,
) -> Result<(hhmc_landmarks::LandmarkLayer, [u8; 32])> {
    let (paths, cache) = (cx.paths, cx.cache);
    let cfg = hhmc_landmarks::LandmarksConfig::load(&paths.root)?;
    let mut kb = KeyBuilder::new("landmarks", hhmc_landmarks::LANDMARKS_VERSION);
    kb.bytes(gkey).file(&paths.config().join("world.toml"))?;
    let cfg_path = hhmc_landmarks::LandmarksConfig::path(&paths.root);
    if cfg_path.exists() {
        kb.file(&cfg_path)?;
    }
    for l in &cfg.landmark {
        kb.file(&paths.root.join(&l.schematic))?;
    }
    let key = kb.finish();
    if !force && let Some(l) = cache.get("landmarks", t, &key) {
        return Ok((l, key));
    }
    let placed = hhmc_landmarks::place_all(&paths.root, &cfg, &cx.world.origin, &cx.world.height)?;
    let l = hhmc_landmarks::landmark_layer(&placed, g, &cx.world.origin);
    if !l.names.is_empty() {
        info!(tile = %t, landmarks = ?l.names, "landmarks placed");
    }
    cache.put("landmarks", t, &key, &l)?;
    Ok((l, key))
}

/// Building layer of a tile, from the cache when its key is unchanged. Buildings on
/// landing stages stand on the infra layer's floating decks.
pub fn building_layer(
    cx: &StageCtx,
    g: &GroundModel,
    gkey: &[u8; 32],
    il: &hhmc_infra::InfraLayer,
    ikey: &[u8; 32],
    t: TileKey,
    force: bool,
) -> Result<(hhmc_buildings::BuildingLayer, [u8; 32])> {
    let (paths, cache, world, blocks) = (cx.paths, cx.cache, cx.world, cx.blocks);
    let bin = paths.prepared().join("buildings").join(format!("{}_{}.bin", t.rx, t.rz));
    let key = KeyBuilder::new("buildings", hhmc_buildings::BUILDINGS_VERSION)
        .bytes(gkey)
        .bytes(ikey)
        .file(&bin)?
        .file(&paths.config().join("blocks.toml"))?
        .file(&paths.config().join("world.toml"))?
        .file(&paths.config().join("facade.toml"))?
        .file(&paths.config().join("block_colors.toml"))?
        .file(&paths.prepared().join("dop20").join(format!("{}_{}.tif", t.rx, t.rz)))?
        .file(&paths.sources_toml())?
        .finish();
    if !force && let Some(l) = cache.get("buildings", t, &key) {
        return Ok((l, key));
    }
    let meshes = if bin.exists() { hhmc_buildings::load_meshes(&bin)? } else { Vec::new() };
    let facade = hhmc_buildings::facade::Facade::load(&paths.root)?;
    let dop = paths.prepared().join("dop20").join(format!("{}_{}.tif", t.rx, t.rz));
    let ortho = if dop.exists() { Some(hhmc_core::raster::read_rgba(&dop)?) } else { None };
    let l = hhmc_buildings::building_layer(
        &meshes,
        t,
        &world.origin,
        g,
        &blocks.buildings,
        Some(&facade),
        ortho.as_ref(),
        Some(&il.floating_tops(cx.infra)),
    );
    cache.put("buildings", t, &key, &l)?;
    Ok((l, key))
}

/// Vegetation layer of a tile, from the cache when its key is unchanged.
pub fn veg_layer(
    cx: &StageCtx,
    g: &GroundModel,
    il: &hhmc_infra::InfraLayer,
    bl: &hhmc_buildings::BuildingLayer,
    bkey: &[u8; 32],
    t: TileKey,
    force: bool,
) -> Result<(hhmc_veg::VegLayer, [u8; 32])> {
    let (paths, cache) = (cx.paths, cx.cache);
    let list = TileTrees::path(paths, t);
    let key = KeyBuilder::new("vegetation", hhmc_veg::VEG_VERSION)
        .bytes(bkey)
        .file(&list)?
        .file(&VegetationConfig::path(paths))?
        .file(&TileInfra::path(paths, t))?
        .file(&InfraConfig::path(paths))?
        .file(&paths.config().join("blocks.toml"))?
        .file(&paths.config().join("world.toml"))?
        .finish();
    if !force && let Some(l) = cache.get("vegetation", t, &key) {
        return Ok((l, key));
    }
    let trees = TileTrees::load(&list).with_context(|| format!("tree list of tile {t}"))?;
    let tops = hhmc_veg::building_tops(&bl.clears, &bl.blocks);
    let structures = TileInfra::path(paths, t);
    let corridor = hhmc_core::infra::track_corridor(
        &TileInfra::load(&structures)?.tracks,
        &cx.world.origin,
        cx.infra.rails.corridor_m,
    );
    let l = hhmc_veg::vegetation(&hhmc_veg::VegInputs {
        trees: &trees,
        cfg: cx.veg,
        blocks: cx.blocks,
        origin: &cx.world.origin,
        ground: g,
        building_top: &tops,
        track_corridor: &corridor,
        stands_on: &il.abutment_tops(),
    })?;
    cache.put("vegetation", t, &key, &l)?;
    Ok((l, key))
}

pub fn build(paths: &Paths, tiles: &[TileKey], clip: Option<&Clip>, force: bool) -> Result<()> {
    let world = WorldConfig::load(paths)?;
    let blocks = BlocksConfig::load(paths)?;
    let veg = VegetationConfig::load(paths)?;
    let infra = InfraConfig::load(paths)?;
    let water = WaterConfig::load(paths)?;
    let bodies = BodiesFile::load(paths)?;
    let cache = Cache::new(paths.cache());
    let layout = WorldLayout::new(paths.out().join("world"));
    let (sx, sz) = world.origin.block_at(world.spawn.e, world.spawn.n);
    if !layout.level_dat().exists() {
        let tpl = crate::world::template(paths, false)?;
        assemble_world(
            &tpl,
            &layout,
            [sx, 64, sz],
            "Hamburg 1:1",
            &crate::world::game_rules(&world)?,
        )?;
    }
    let inp = GroundInputs {
        paths,
        blocks: &blocks,
        water: &water,
        bodies: &bodies,
        height: &world.height,
    };
    let spawn_tile = TileKey::from_block(sx, sz);
    let start = Instant::now();
    let report_dir = paths.out().join("build").join("tiles");
    std::fs::create_dir_all(&report_dir)?;
    let results: Vec<(TileKey, bool, Option<i32>)> = tiles
        .par_iter()
        .map(|&t| -> Result<_> {
            let tile_start = Instant::now();
            let cx = StageCtx {
                paths,
                cache: &cache,
                world: &world,
                blocks: &blocks,
                veg: &veg,
                infra: &infra,
                clip,
            };
            let l = layers(&cx, &inp, t, force)?;
            let ckey = compose_key(&l, paths, &world)?;
            let key_file = paths.cache().join("compose").join(format!("{}_{}.key", t.rx, t.rz));
            let region = layout.region_file(t);
            let fresh =
                !force && region.exists() && std::fs::read(&key_file).is_ok_and(|k| k == ckey);
            if !fresh {
                let vt = compose(&l, &blocks)?;
                hhmc_world::write_tile(&vt, &layout.region_dir(), world.minecraft.data_version)?;
                std::fs::create_dir_all(key_file.parent().expect("has parent"))?;
                std::fs::write(&key_file, ckey)?;
                // Round trip: columns of the written region read back against the tile.
                let back = hhmc_world::read_tile(&region, t)?;
                let (columns, mismatches) = round_trip(&vt, &back, ROUND_TRIP_COLUMNS);
                let report = TileBuildReport {
                    tile: [t.rx, t.rz],
                    secs: tile_start.elapsed().as_secs_f64(),
                    forced: force,
                    round_trip_columns: columns,
                    round_trip_mismatches: mismatches,
                };
                std::fs::write(
                    report_dir.join(format!("{}_{}.json", t.rx, t.rz)),
                    serde_json::to_string_pretty(&report)? + "\n",
                )?;
            }
            let (g, bl) = (&l.g, &l.bl);
            let spawn_y = (t == spawn_tile).then(|| {
                let (bx, bz) = t.min_block();
                let i = g.idx((sx - bx) as usize, (sz - bz) as usize);
                let top = if g.water_top[i] != NO_WATER { g.water_top[i] } else { g.ground[i] };
                // Above any building at the spawn column.
                let (lx, lz) = ((sx - bx) as u16, (sz - bz) as u16);
                let roof = bl.blocks.iter().filter(|b| (b.0, b.2) == (lx, lz)).map(|b| b.1).max();
                i32::from(roof.unwrap_or(top).max(top)) + 1
            });
            Ok((t, !fresh, spawn_y))
        })
        .collect::<Result<_>>()?;
    let written = results.iter().filter(|r| r.1).count();
    if let Some(y) = results.iter().find_map(|r| r.2) {
        patch_level_dat(&layout.level_dat(), [sx, y, sz], "Hamburg 1:1")?;
        info!(x = sx, y, z = sz, "spawn set");
    }
    let peak_rss_mb = peak_rss_mb();
    info!(
        tiles = tiles.len(),
        written,
        secs = format!("{:.1}", start.elapsed().as_secs_f64()),
        peak_rss_mb = ?peak_rss_mb,
        world = %layout.root.display(),
        "build done"
    );
    if written > 0 {
        let run = BuildRunReport {
            tiles: tiles.len(),
            written,
            forced: force,
            secs: start.elapsed().as_secs_f64(),
            peak_rss_mb,
            threads: rayon::current_num_threads(),
        };
        std::fs::write(
            paths.out().join("build").join("run.json"),
            serde_json::to_string_pretty(&run)? + "\n",
        )?;
    }
    Ok(())
}

/// Columns compared in the round trip of each written tile.
const ROUND_TRIP_COLUMNS: usize = 1000;

/// What `build` records per written tile (out/build/tiles/), for the QA report.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct TileBuildReport {
    pub tile: [i32; 2],
    /// Wall time of the tile's stages, compose, write and round trip (tiles build in
    /// parallel, so this includes contention).
    pub secs: f64,
    /// Built with `--force` (no stage from the cache): comparable with the baseline.
    pub forced: bool,
    pub round_trip_columns: usize,
    pub round_trip_mismatches: usize,
}

/// The last build run that wrote tiles (out/build/run.json).
#[derive(serde::Serialize, serde::Deserialize)]
pub struct BuildRunReport {
    pub tiles: usize,
    pub written: usize,
    pub forced: bool,
    pub secs: f64,
    /// Peak resident memory of the build process (Linux; none elsewhere).
    pub peak_rss_mb: Option<u64>,
    pub threads: usize,
}

/// Peak resident memory of this process in MiB, from /proc (Linux only).
fn peak_rss_mb() -> Option<u64> {
    let s = std::fs::read_to_string("/proc/self/status").ok()?;
    let kb: u64 = s
        .lines()
        .find_map(|l| l.strip_prefix("VmHWM:"))?
        .trim()
        .trim_end_matches(" kB")
        .trim()
        .parse()
        .ok()?;
    Some(kb / 1024)
}

/// Compares `n` columns (chosen deterministically) of two tiles block by block; returns
/// (columns compared, columns that differ).
pub fn round_trip(a: &hhmc_core::VoxelTile, b: &hhmc_core::VoxelTile, n: usize) -> (usize, usize) {
    let size = hhmc_core::TILE_SIZE as u64;
    let mut state: u64 = 0x9E37_79B9_7F4A_7C15 ^ ((a.key.rx as u64) << 32 | a.key.rz as u32 as u64);
    let mut bad = 0;
    for _ in 0..n {
        // splitmix64
        state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        let (x, zz) = ((z % size) as usize, ((z >> 32) % size) as usize);
        if !columns_equal(a, b, x, zz) {
            bad += 1;
        }
    }
    (n, bad)
}

/// Whether column (x, z) holds the same block states in both tiles.
pub fn columns_equal(
    a: &hhmc_core::VoxelTile,
    b: &hhmc_core::VoxelTile,
    x: usize,
    z: usize,
) -> bool {
    (hhmc_core::voxel::MIN_Y..hhmc_core::voxel::MAX_Y_EXCL)
        .all(|y| a.palette.get(a.get(x, y, z)) == b.palette.get(b.get(x, y, z)))
}

/// All stage layers of a tile (from the cache unless `force`).
pub struct Layers {
    pub g: GroundModel,
    pub gkey: [u8; 32],
    pub il: hhmc_infra::InfraLayer,
    pub ikey: [u8; 32],
    pub bl: hhmc_buildings::BuildingLayer,
    pub bkey: [u8; 32],
    pub vl: hhmc_veg::VegLayer,
    pub vkey: [u8; 32],
    pub ll: hhmc_landmarks::LandmarkLayer,
    pub lkey: [u8; 32],
    /// Inside flags of the columns when the area is clipped (see [`Clip::mask`]).
    pub inside: Option<Vec<bool>>,
    pub clip_key: Option<[u8; 32]>,
}

pub fn layers(cx: &StageCtx, inp: &GroundInputs, t: TileKey, force: bool) -> Result<Layers> {
    let (g, gkey) = ground_model(cx.paths, cx.cache, inp, t, force)?;
    let (il, ikey) = infra_layer(cx, &g, &gkey, t, force)?;
    let (bl, bkey) = building_layer(cx, &g, &gkey, &il, &ikey, t, force)?;
    let (vl, vkey) = veg_layer(cx, &g, &il, &bl, &bkey, t, force)?;
    let (ll, lkey) = landmark_layer(cx, &g, &gkey, t, force)?;
    let inside = cx.clip.and_then(|c| c.mask(cx.world, t));
    let clip_key = cx.clip.map(|c| c.key);
    Ok(Layers { g, gkey, il, ikey, bl, bkey, vl, vkey, ll, lkey, inside, clip_key })
}

/// Cache key of the composed tile.
pub fn compose_key(l: &Layers, paths: &Paths, world: &WorldConfig) -> Result<[u8; 32]> {
    Ok(KeyBuilder::new("compose", COMPOSE_VERSION)
        .bytes(&l.gkey)
        .bytes(&l.ikey)
        .bytes(&l.bkey)
        .bytes(&l.vkey)
        .bytes(&l.lkey)
        .bytes(l.clip_key.as_ref().map_or(&[][..], |k| &k[..]))
        .file(&paths.config().join("blocks.toml"))?
        .bytes(&world.minecraft.data_version.to_le_bytes())
        .finish())
}

/// The tile's blocks: ground, then vegetation, infrastructure, buildings and landmarks.
pub fn compose(l: &Layers, blocks: &BlocksConfig) -> Result<hhmc_core::VoxelTile> {
    let (vl, il, bl) = (&l.vl, &l.il, &l.bl);
    let mut vt = compose_ground(&l.g, blocks)?;
    // Plan order: infrastructure wins over trees, buildings over both.
    hhmc_world::compose::apply_layer(&mut vt, &vl.palette, &[], &vl.blocks);
    hhmc_world::compose::apply_layer(&mut vt, &il.palette, &il.clears, &il.blocks);
    hhmc_world::compose::apply_layer(&mut vt, &bl.palette, &bl.clears, &bl.blocks);
    // Railways through stations.
    hhmc_world::compose::apply_layer(
        &mut vt,
        &il.palette,
        &il.override_clears,
        &il.override_blocks,
    );
    // Landmarks over everything generated.
    hhmc_world::compose::apply_layer(&mut vt, &l.ll.palette, &l.ll.clears, &l.ll.blocks);
    // Outside the area's boundary nothing: the world ends at the city limits.
    if let Some(inside) = &l.inside {
        let n = TILE_SIZE as usize;
        for (i, _) in inside.iter().enumerate().filter(|(_, v)| !**v) {
            vt.fill_column(i % n, i / n, MIN_Y, MAX_Y_EXCL - 1, hhmc_core::AIR);
        }
        vt.prune();
    }
    Ok(vt)
}
