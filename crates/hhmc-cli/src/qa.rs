//! `hhmc qa`: checks on the written world against the prepared data (out/qa/).

use anyhow::{Result, bail};
use hhmc_core::cache::Cache;
use hhmc_core::landcfg::{BlocksConfig, BodiesFile, WaterConfig};
use hhmc_core::{Paths, TileKey, WorldConfig};
use hhmc_qa::ground_qa::{BuildingQaResult, GroundQaResult};
use tracing::info;

use crate::build;

pub fn qa(paths: &Paths, area: &str, tiles: &[TileKey], set_baseline: bool) -> Result<()> {
    let qa_start = std::time::Instant::now();
    let cfg = WorldConfig::load(paths)?;
    let blocks = BlocksConfig::load(paths)?;
    let water = WaterConfig::load(paths)?;
    let bodies = BodiesFile::load(paths)?;
    let cache = Cache::new(paths.cache());
    // `area` names an area, or is the --tiles list (never clipped).
    let clip = if hhmc_core::Areas::load(paths)?.0.contains_key(area) {
        build::Clip::for_area(paths, Some(area))?
    } else {
        None
    };
    let layout = hhmc_world::WorldLayout::new(paths.out().join("world"));
    let inp = hhmc_ground::GroundInputs {
        paths,
        blocks: &blocks,
        water: &water,
        bodies: &bodies,
        height: &cfg.height,
    };
    let veg = hhmc_core::trees::VegetationConfig::load(paths)?;
    let infra = hhmc_core::infra::InfraConfig::load(paths)?;
    let cx = build::StageCtx {
        paths,
        cache: &cache,
        world: &cfg,
        blocks: &blocks,
        veg: &veg,
        infra: &infra,
        clip: clip.as_ref(),
    };
    let layer_of = |tk| -> Result<hhmc_qa::ground_qa::StageLayers> {
        let unclipped = match clip.as_ref().and_then(|c| c.mask(&cfg, tk)) {
            Some(inside) => {
                let mut l = build::layers(&cx, &inp, tk, false)?;
                l.inside = None;
                Some((inside, build::compose(&l, &blocks)?))
            }
            None => None,
        };
        let (g, gkey) = build::ground_model(paths, &cache, &inp, tk, false)?;
        let (il, ikey) = build::infra_layer(&cx, &g, &gkey, tk, false)?;
        let l = build::building_layer(&cx, &g, &gkey, &il, &ikey, tk, false)?.0;
        let ll = build::landmark_layer(&cx, &g, &gkey, tk, false)?.0;
        Ok(hhmc_qa::ground_qa::StageLayers {
            buildings: Some(l),
            ground: Some(g),
            infra: Some(il),
            landmarks: Some(ll),
            unclipped,
        })
    };
    let res = hhmc_qa::ground_qa::run(
        paths,
        tiles,
        &cfg.origin,
        &cfg.height,
        &layout.region_dir(),
        &layer_of,
    )?;

    let g = |f: fn(&GroundQaResult) -> usize| res.iter().map(f).sum::<usize>();
    info!(
        tiles = res.len(),
        failed = res.iter().filter(|r| !r.pass).count(),
        land_columns = g(|r| r.land_columns),
        land_fail = g(|r| r.land_fail),
        water_columns = g(|r| r.water_columns),
        water_holes = g(|r| r.water_holes),
        seam_walls = g(|r| r.seam_walls),
        water_walls = g(|r| r.water_walls),
        ground_holes = g(|r| r.ground_holes),
        outside_columns = g(|r| r.outside_columns),
        outside_nonempty = g(|r| r.outside_nonempty),
        "ground QA"
    );
    let tq = |f: fn(&hhmc_qa::tree_qa::TreeQaResult) -> usize| {
        res.iter().map(|r| f(&r.trees)).sum::<usize>()
    };
    info!(
        cadaster = tq(|r| r.cadaster),
        placed = tq(|r| r.placed),
        conflicts = tq(|r| r.conflicts),
        unaccounted = tq(|r| r.unaccounted_count),
        canopy = tq(|r| r.canopy),
        trunks_missing = tq(|r| r.trunks_missing_count),
        tree_voxels = tq(|r| r.tree_voxels),
        in_building = tq(|r| r.in_building),
        in_water = tq(|r| r.in_water),
        on_road = tq(|r| r.on_road),
        "tree QA"
    );
    let iq = |f: fn(&hhmc_qa::infra_qa::InfraQaResult) -> usize| {
        res.iter().map(|r| f(&r.infra)).sum::<usize>()
    };
    info!(
        deck_columns = iq(|r| r.deck_columns),
        elevated_track_cells = iq(|r| r.elevated_track_cells),
        elevated_gaps = iq(|r| r.elevated_gaps),
        elevated_steps = iq(|r| r.elevated_steps),
        bridge_water_columns = iq(|r| r.bridge_water_columns),
        bridge_blocked = iq(|r| r.bridge_blocked),
        street_contacts = iq(|r| r.street_contacts),
        street_steps = iq(|r| r.street_steps),
        street_rises = iq(|r| r.street_rises),
        ground_rail_cells = iq(|r| r.ground_rail_cells),
        ground_rail_steps = iq(|r| r.ground_rail_steps),
        surface_joints = iq(|r| r.surface_joints),
        surface_mismatches = iq(|r| r.surface_mismatches),
        bridge_decks = iq(|r| r.bridge_decks),
        isolated_decks = iq(|r| r.isolated_decks),
        one_sided_decks = iq(|r| r.one_sided_decks),
        deck_cliffs = iq(|r| r.deck_cliffs),
        floating_columns = iq(|r| r.floating_columns),
        floating_off_level = iq(|r| r.floating_off_level),
        "infra QA"
    );
    let b = |f: fn(&BuildingQaResult) -> usize| res.iter().map(|r| f(&r.buildings)).sum::<usize>();
    let mut med: Vec<f64> = res
        .iter()
        .filter(|r| r.buildings.roof_columns > 0)
        .map(|r| r.buildings.roof_median_abs_m)
        .collect();
    med.sort_by(f64::total_cmp);
    info!(
        building_columns = b(|r| r.columns),
        odd_columns = b(|r| r.odd_columns),
        odd_buildings = b(|r| r.odd_buildings.len()),
        textured_buildings = b(|r| r.textured_buildings),
        curtain_voxels = b(|r| r.curtain_voxels),
        window_voxels = b(|r| r.window_voxels),
        textured_wall_voxels = b(|r| r.textured_wall_voxels),
        floating = b(|r| r.floating.len()),
        floating_on_water = b(|r| r.floating_on_water),
        roof_changed = b(|r| r.roof_changed.len()),
        raw_sunk = b(|r| r.raw_sunk),
        raw_floating = b(|r| r.raw_floating),
        roof_columns = b(|r| r.roof_columns),
        median_tile_roof_err_m = med.get(med.len() / 2).copied().unwrap_or(0.0),
        worst_tile_roof_err_m = med.last().copied().unwrap_or(0.0),
        "building QA"
    );
    // Area-wide roof median from the per-tile histograms (target ≤ 1 m).
    let mut hist = vec![0u64; 501];
    for r in &res {
        for (i, c) in r.buildings.roof_abs_hist.iter().enumerate() {
            hist[i] += u64::from(*c);
        }
    }
    let total: u64 = hist.iter().sum();
    let mut acc = 0;
    let roof_median = hist.iter().position(|&c| {
        acc += c;
        acc * 2 >= total
    });
    let roof_median_m = roof_median.map_or(0.0, |i| i as f64 / 10.0);
    let qcfg = hhmc_qa::ground_qa::load_config(paths)?;
    let review: Vec<_> = res.iter().filter(|r| r.roof_review).map(|r| r.tile).collect();
    info!(roof_median_m, limit = qcfg.roof_median_abs_max_m, review_tiles = ?review, "area roof height vs bDOM");
    // Roof color vs DOP20, area-wide median ΔE2000 for textured buildings.
    let mut dh = vec![0u64; 101];
    for r in &res {
        for (i, c) in r.buildings.roof_de_hist.iter().enumerate() {
            dh[i] += u64::from(*c);
        }
    }
    let dt: u64 = dh.iter().sum();
    let mut acc = 0;
    let de_median = dh.iter().position(|&c| {
        acc += c;
        acc * 2 >= dt
    });
    let mut untex: Vec<f32> =
        res.iter().map(|r| r.buildings.roof_de_untextured_median).filter(|v| *v > 0.0).collect();
    untex.sort_by(f32::total_cmp);
    info!(
        roof_de2000_median_textured = ?de_median,
        roof_de2000_tile_median_untextured = untex.get(untex.len() / 2).copied().unwrap_or(0.0),
        "roof color vs DOP20"
    );
    let missing = coverage(paths, &res)?;
    info!(
        missing = missing.len(),
        "building coverage: LoD2 buildings whose geometry produced no voxels"
    );
    for m in missing.iter().take(20) {
        eprintln!("MISSING building {m}");
    }
    let failed: Vec<_> = res.iter().filter(|r| !r.pass).collect();
    for r in &failed {
        let mut why = Vec::new();
        for (n, v) in [
            ("land off DGM1", r.land_fail),
            ("water holes", r.water_holes),
            ("seam walls", r.seam_walls),
            ("water walls", r.water_walls),
            ("ground holes", r.ground_holes),
            ("floating buildings", r.buildings.floating.len()),
            ("elevated rail gaps", r.infra.elevated_gaps),
            ("elevated rail steps", r.infra.elevated_steps),
            ("ground rail steps", r.infra.ground_rail_steps),
            ("bridges blocking water", r.infra.bridge_blocked),
            ("pontoons off level", r.infra.floating_off_level),
            ("empty decks", r.infra.empty_decks.len()),
        ] {
            if v > 0 {
                why.push(format!("{n} {v}"));
            }
        }
        if !r.trees.pass {
            why.push("tree checks".into());
        }
        if r.surface_off_fraction > qcfg.surface_max_off_fraction {
            why.push(format!("surface off bDOM {:.1} %", 100.0 * r.surface_off_fraction));
        }
        eprintln!("FAIL tile {},{}: {}", r.tile[0], r.tile[1], why.join(", "));
    }
    let roof_fail = total > 0 && roof_median_m > qcfg.roof_median_abs_max_m;
    #[derive(serde::Deserialize)]
    struct Color {
        roof_de2000_median_max: f64,
        min_textured_buildings: usize,
    }
    #[derive(serde::Deserialize)]
    struct QaColor {
        color: Color,
    }
    let cq: QaColor = hhmc_core::config::load_toml(&paths.config().join("qa.toml"))?;
    // A median over a handful of buildings says little: judged from enough of them.
    let textured: usize = res.iter().map(|r| r.buildings.textured_buildings).sum();
    let color_fail = textured >= cq.color.min_textured_buildings
        && de_median.is_some_and(|m| m as f64 > cq.color.roof_de2000_median_max);
    if color_fail {
        eprintln!(
            "FAIL roof color: median ΔE2000 {de_median:?} > {}",
            cq.color.roof_de2000_median_max
        );
    }
    let roof_fail = roof_fail || color_fail;

    // Build reports (round trip, time), the performance baseline, determinism and the
    // last server test, then the HTML report.
    #[derive(serde::Deserialize)]
    struct Phase7 {
        determinism: Determinism,
        performance: Performance,
    }
    #[derive(serde::Deserialize)]
    struct Determinism {
        tiles: Vec<[i32; 2]>,
    }
    #[derive(serde::Deserialize)]
    struct Performance {
        slower_than: f64,
    }
    let p7: Phase7 = hhmc_core::config::load_toml(&paths.config().join("qa.toml"))?;
    let mut builds = std::collections::BTreeMap::new();
    for t in tiles {
        let f = paths.out().join("build/tiles").join(format!("{}_{}.json", t.rx, t.rz));
        if let Ok(s) = std::fs::read_to_string(&f) {
            let r: build::TileBuildReport = serde_json::from_str(&s)?;
            builds.insert([t.rx, t.rz], r);
        }
    }
    let baseline_file = paths.config().join("perf_baseline.toml");
    if set_baseline {
        let forced: Vec<_> = builds.values().filter(|b| b.forced).collect();
        if forced.len() != tiles.len() {
            bail!(
                "--set-baseline needs a forced build of every tile (`just build --area <area> --force`)"
            );
        }
        let mut s = String::from(
            "# Per-tile build seconds of a forced build, the baseline of the QA performance check\n\
             # (`hhmc qa --set-baseline` after `hhmc build --force`). Machine-dependent.\n",
        );
        s += &format!("threads = {}\n\n[tiles]\n", rayon::current_num_threads());
        for b in forced {
            s += &format!("\"{}_{}\" = {:.2}\n", b.tile[0], b.tile[1], b.secs);
        }
        std::fs::write(&baseline_file, s)?;
        info!(file = %baseline_file.display(), "performance baseline recorded");
    }
    let baseline: Option<std::collections::BTreeMap<[i32; 2], f64>> = if baseline_file.exists() {
        #[derive(serde::Deserialize)]
        struct B {
            tiles: std::collections::BTreeMap<String, f64>,
        }
        let b: B = hhmc_core::config::load_toml(&baseline_file)?;
        Some(
            b.tiles
                .into_iter()
                .filter_map(|(k, v)| {
                    let (x, z) = k.split_once('_')?;
                    Some(([x.parse().ok()?, z.parse().ok()?], v))
                })
                .collect(),
        )
    } else {
        None
    };
    let mut determinism = Vec::new();
    let layout = hhmc_world::WorldLayout::new(paths.out().join("world"));
    for d in &p7.determinism.tiles {
        let t = TileKey::new(d[0], d[1]);
        if !tiles.contains(&t) {
            continue;
        }
        let l = build::layers(&cx, &inp, t, true)?;
        let fresh = build::compose(&l, &blocks)?;
        let disk = hhmc_world::read_tile(&layout.region_file(t), t)?;
        let size = hhmc_core::TILE_SIZE as usize;
        let differ = (0..size * size)
            .filter(|&k| !build::columns_equal(&fresh, &disk, k % size, k / size))
            .count();
        info!(tile = %t, differ, "determinism");
        determinism.push((*d, size * size, differ));
    }
    let server = std::fs::read_to_string(paths.out().join("qa/server.json")).ok().and_then(|s| {
        let v: serde_json::Value = serde_json::from_str(&s).ok()?;
        let st: Vec<[i32; 2]> = serde_json::from_value(v["tiles"].clone()).ok()?;
        // Only a test of these tiles counts.
        if !tiles.iter().all(|t| st.contains(&[t.rx, t.rz])) {
            return None;
        }
        let when = v["unix_time"].as_u64().unwrap_or(0);
        let world_written = std::fs::metadata(layout.level_dat())
            .and_then(|m| m.modified())
            .ok()
            .and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_secs());
        let stale = if when < world_written { ", older than the world" } else { "" };
        Some((
            v["passed"].as_bool().unwrap_or(false) && stale.is_empty(),
            st.len(),
            v["report"]["asserts"].as_u64().unwrap_or(0) as usize,
            format!("unix time {when}{stale}"),
        ))
    });
    let surface_max = qcfg.surface_max_off_fraction;
    crate::report::write(
        &paths.out().join("qa"),
        &res,
        &crate::report::AreaResults {
            area,
            roof_median_m,
            roof_median_max_m: qcfg.roof_median_abs_max_m,
            roof_de_median: de_median,
            roof_de_max: cq.color.roof_de2000_median_max,
            roof_de_judged: textured >= cq.color.min_textured_buildings,
            missing_buildings: &missing,
            builds: &builds,
            baseline: baseline.as_ref(),
            slower_than: p7.performance.slower_than,
            determinism: &determinism,
            server,
            surface_max,
            qa_secs: qa_start.elapsed().as_secs_f64(),
        },
    )?;
    info!(report = %paths.out().join("qa/index.html").display(), secs = format!("{:.1}", qa_start.elapsed().as_secs_f64()), "QA report written");
    let round_trip_bad = builds.values().filter(|b| b.round_trip_mismatches > 0).count();
    let det_bad = determinism.iter().filter(|d| d.2 > 0).count();
    if round_trip_bad > 0 || det_bad > 0 {
        bail!(
            "QA failed: {round_trip_bad} tiles fail the round trip, {det_bad} the determinism check"
        );
    }
    if !failed.is_empty() || !missing.is_empty() || roof_fail {
        bail!(
            "QA failed: {} tiles, {} buildings missing, area roof median {roof_median_m} m",
            failed.len(),
            missing.len()
        );
    }
    Ok(())
}

/// Buildings whose geometry produced no voxels (from every tile they were binned into).
fn coverage(_paths: &Paths, res: &[GroundQaResult]) -> Result<Vec<String>> {
    let mut v: Vec<String> = res.iter().flat_map(|r| r.buildings.empty.iter().cloned()).collect();
    v.sort();
    v.dedup();
    Ok(v)
}

/// Renders every `[[viewpoint]]` of config/qa.toml.
pub fn views(
    paths: &Paths,
    width: usize,
    config: Option<std::path::PathBuf>,
    out_dir: Option<std::path::PathBuf>,
) -> Result<()> {
    #[derive(serde::Deserialize)]
    struct Q {
        viewpoint: Vec<hhmc_qa::render::Viewpoint>,
    }
    let q: Q =
        hhmc_core::config::load_toml(&config.unwrap_or_else(|| paths.config().join("qa.toml")))?;
    let cfg = WorldConfig::load(paths)?;
    let colors = hhmc_qa::palette::load(&paths.config().join("block_colors.toml"))?;
    let region_dir = hhmc_world::WorldLayout::new(paths.out().join("world")).region_dir();
    let out = out_dir.unwrap_or_else(|| paths.out().join("qa/views"));
    std::fs::create_dir_all(&out)?;
    let height = width * 9 / 16;
    let mut md = String::from("# QA viewpoints\n\n");
    for (i, v) in q.viewpoint.iter().enumerate() {
        let cam = hhmc_qa::render::Camera::from_viewpoint(v, &cfg.origin);
        let (cx, cz) = (cam.pos[0] as i32, cam.pos[2] as i32);
        let r = 1;
        let tiles: Vec<TileKey> = (-r..=r)
            .flat_map(|dz| (-r..=r).map(move |dx| (dx, dz)))
            .map(|(dx, dz)| {
                TileKey::new(
                    TileKey::from_block(cx, cz).rx + dx,
                    TileKey::from_block(cx, cz).rz + dz,
                )
            })
            .collect();
        let world = hhmc_qa::render::World::load(&region_dir, &tiles, &colors)?;
        let img = hhmc_qa::render::render(&world, &cam, width, height, 700.0);
        let name = format!("{:02}.png", i + 1);
        hhmc_qa::ground_qa::write_png(&out.join(&name), width, height, &img)?;
        md.push_str(&format!(
            "## {}\n\n![{}]({name})\n\n`{}`\n\n",
            v.name,
            v.name,
            cam.tp_command()
        ));
        info!(view = %v.name, file = %out.join(&name).display(), tp = %cam.tp_command(), "rendered");
    }
    std::fs::write(out.join("README.md"), md)?;
    Ok(())
}
