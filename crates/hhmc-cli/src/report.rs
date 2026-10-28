//! The QA report: `out/qa/index.html` with a per-tile pass/fail map, the 20 worst tiles
//! per check and a page per tile with its images and findings.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;

use anyhow::Result;
use hhmc_core::{TILE_SIZE, TileKey};
use hhmc_qa::ground_qa::GroundQaResult;

use crate::build::TileBuildReport;

/// Results that are not per tile.
pub struct AreaResults<'a> {
    pub area: &'a str,
    pub roof_median_m: f64,
    pub roof_median_max_m: f64,
    pub roof_de_median: Option<usize>,
    pub roof_de_max: f64,
    /// Enough textured buildings for the area color median to count.
    pub roof_de_judged: bool,
    pub missing_buildings: &'a [String],
    /// Per tile build reports (round trip, time), by tile.
    pub builds: &'a BTreeMap<[i32; 2], TileBuildReport>,
    /// Per-tile build seconds of the baseline, by tile.
    pub baseline: Option<&'a BTreeMap<[i32; 2], f64>>,
    pub slower_than: f64,
    /// Determinism: (tile, columns compared, columns differing).
    pub determinism: &'a [([i32; 2], usize, usize)],
    /// The last server test of the built world: (passed, tiles, asserts, summary).
    pub server: Option<(bool, usize, usize, String)>,
    pub surface_max: f64,
    pub qa_secs: f64,
}

/// One per-tile check: its flag, its ranking metric and a one-line value.
struct Check {
    id: &'static str,
    title: &'static str,
    what: &'static str,
    flag: Box<dyn Fn(&GroundQaResult) -> bool>,
    metric: Box<dyn Fn(&GroundQaResult) -> f64>,
    value: Box<dyn Fn(&GroundQaResult) -> String>,
}

fn checks(a: &AreaResults) -> Vec<Check> {
    let surface_max = a.surface_max;
    let builds: BTreeMap<[i32; 2], (f64, usize, bool)> =
        a.builds.iter().map(|(k, b)| (*k, (b.secs, b.round_trip_mismatches, b.forced))).collect();
    let builds2 = builds.clone();
    let base: BTreeMap<[i32; 2], f64> = a.baseline.cloned().unwrap_or_default();
    let base2 = base.clone();
    let slower = a.slower_than;
    let slow_ratio = move |t: &GroundQaResult| -> Option<f64> {
        let (secs, _, forced) = builds.get(&t.tile)?;
        let b = base.get(&t.tile)?;
        forced.then_some(secs / b)
    };
    let slow_ratio2 = slow_ratio.clone();
    let slow_ratio3 = slow_ratio.clone();
    let ground = |t: &GroundQaResult| {
        t.land_fail
            + t.water_holes
            + t.seam_walls
            + t.water_walls
            + t.ground_holes
            + t.outside_nonempty
    };
    let tree = |t: &GroundQaResult| {
        let r = &t.trees;
        r.in_building + r.in_water + r.on_road + r.trunks_missing_count + r.unaccounted_count
    };
    let infra = |t: &GroundQaResult| {
        let r = &t.infra;
        r.elevated_gaps
            + r.elevated_steps
            + r.ground_rail_steps
            + r.bridge_blocked
            + r.floating_off_level
            + r.empty_decks.len()
    };
    let infra_review = |t: &GroundQaResult| {
        let r = &t.infra;
        r.street_steps + r.surface_mismatches + r.isolated_decks + r.one_sided_decks
    };
    let coverage = |t: &GroundQaResult| {
        t.buildings.empty.len() + t.infra.empty_decks.len() + t.trees.unaccounted_count
    };
    vec![
        Check {
            id: "ground",
            title: "Ground and water",
            what: "Land columns off DGM1 by more than the tolerance, water holes, DGM1/DGM-W seam walls, water walls, holes in the ground, and blocks outside the area's boundary.",
            flag: Box::new(move |t| ground(t) > 0),
            metric: Box::new(move |t| ground(t) as f64),
            value: Box::new(|t| {
                let outside = if t.outside_columns > 0 {
                    format!(" · non-empty outside {} of {}", t.outside_nonempty, t.outside_columns)
                } else {
                    String::new()
                };
                format!(
                    "land off {} · water holes {} · seam walls {} · water walls {} · ground holes {}{outside}",
                    t.land_fail, t.water_holes, t.seam_walls, t.water_walls, t.ground_holes
                )
            }),
        },
        Check {
            id: "surface",
            title: "Surface height",
            what: "Share of columns whose highest block is more than 2 m off bDOM (water and tree crowns aside).",
            flag: Box::new(move |t| t.surface_off_fraction > surface_max),
            metric: Box::new(|t| t.surface_off_fraction),
            value: Box::new(|t| {
                format!("{:.1} % of {} columns", 100.0 * t.surface_off_fraction, t.surface_columns)
            }),
        },
        Check {
            id: "roof",
            title: "Roof height",
            what: "Median |highest building block − bDOM| over the tile's building columns (a review flag; the pass criterion is the area median).",
            flag: Box::new(|t| t.roof_review),
            metric: Box::new(|t| t.buildings.roof_median_abs_m),
            value: Box::new(|t| {
                format!(
                    "{:.2} m over {} columns",
                    t.buildings.roof_median_abs_m, t.buildings.roof_columns
                )
            }),
        },
        Check {
            id: "color",
            title: "Roof color",
            what: "Median ΔE2000 between the roof blocks' color and DOP20 (textured buildings; the pass criterion is the area median).",
            flag: Box::new(|_| false),
            metric: Box::new(|t| hist_median(&t.buildings.roof_de_hist).map_or(0.0, |m| m as f64)),
            value: Box::new(|t| match hist_median(&t.buildings.roof_de_hist) {
                Some(m) => format!("median ΔE2000 {m}"),
                None => "no textured roofs".into(),
            }),
        },
        Check {
            id: "floating",
            title: "Floating buildings",
            what: "Buildings whose every column floats above the ground (pontoon buildings over water aside).",
            flag: Box::new(|t| !t.buildings.floating.is_empty()),
            metric: Box::new(|t| t.buildings.floating.len() as f64),
            value: Box::new(|t| format!("{} floating", t.buildings.floating.len())),
        },
        Check {
            id: "coverage",
            title: "Coverage",
            what: "Input objects that produced nothing: LoD2 buildings, ALKIS decks over water, cadaster trees neither placed nor logged.",
            flag: Box::new(move |t| coverage(t) > 0),
            metric: Box::new(move |t| coverage(t) as f64),
            value: Box::new(|t| {
                format!(
                    "buildings {} · decks {} · trees {}",
                    t.buildings.empty.len(),
                    t.infra.empty_decks.len(),
                    t.trees.unaccounted_count
                )
            }),
        },
        Check {
            id: "trees",
            title: "Trees",
            what: "Tree voxels in buildings, water or on roads, trunks missing, cadaster trees unaccounted for.",
            flag: Box::new(|t| !t.trees.pass),
            metric: Box::new(move |t| tree(t) as f64),
            value: Box::new(|t| {
                let r = &t.trees;
                format!(
                    "{} placed · in buildings {} · in water {} · on roads {} · trunks missing {}",
                    r.placed, r.in_building, r.in_water, r.on_road, r.trunks_missing_count
                )
            }),
        },
        Check {
            id: "infra",
            title: "Infrastructure",
            what: "Elevated railway gaps and steps, ground rail steps, bridges blocking water, pontoons off level, decks over water that drew nothing.",
            flag: Box::new(move |t| infra(t) > 0),
            metric: Box::new(move |t| infra(t) as f64),
            value: Box::new(|t| {
                let r = &t.infra;
                format!(
                    "elevated gaps {} · steps {} · ground rail steps {} · bridges blocked {} · pontoons off {}",
                    r.elevated_gaps,
                    r.elevated_steps,
                    r.ground_rail_steps,
                    r.bridge_blocked,
                    r.floating_off_level
                )
            }),
        },
        Check {
            id: "infra-review",
            title: "Bridge joints (review)",
            what: "Steps of 2+ blocks where decks land, lane mismatches, decks without joints or with joints at one end only, cliffs within decks. Not a pass criterion.",
            flag: Box::new(|_| false),
            metric: Box::new(move |t| infra_review(t) as f64 + t.infra.deck_cliffs as f64 * 0.1),
            value: Box::new(|t| {
                let r = &t.infra;
                format!(
                    "landing steps {} · lane mismatches {} · isolated {} · one-sided {} · cliff pairs {}",
                    r.street_steps,
                    r.surface_mismatches,
                    r.isolated_decks,
                    r.one_sided_decks,
                    r.deck_cliffs
                )
            }),
        },
        Check {
            id: "roundtrip",
            title: "Round trip",
            what: "Columns of the written region file read back that differ from the built tile (1000 per tile, at build time).",
            flag: Box::new(move |t| builds2.get(&t.tile).is_some_and(|b| b.1 > 0)),
            metric: Box::new({
                let b = a
                    .builds
                    .iter()
                    .map(|(k, v)| (*k, v.round_trip_mismatches))
                    .collect::<BTreeMap<_, _>>();
                move |t| b.get(&t.tile).copied().unwrap_or(0) as f64
            }),
            value: Box::new({
                let b = a
                    .builds
                    .iter()
                    .map(|(k, v)| (*k, (v.round_trip_columns, v.round_trip_mismatches)))
                    .collect::<BTreeMap<_, _>>();
                move |t| match b.get(&t.tile) {
                    Some((c, m)) => format!("{m} of {c} columns differ"),
                    None => "no build report".into(),
                }
            }),
        },
        Check {
            id: "performance",
            title: "Performance",
            what: "Build time of the tile in the last forced build against the recorded baseline.",
            flag: Box::new(move |t| slow_ratio(t).is_some_and(|r| r > slower)),
            metric: Box::new(move |t| slow_ratio2(t).unwrap_or(0.0)),
            value: Box::new({
                let b = a.builds.iter().map(|(k, v)| (*k, v.secs)).collect::<BTreeMap<_, _>>();
                move |t| match (b.get(&t.tile), base2.get(&t.tile), slow_ratio3(t)) {
                    (Some(s), Some(bs), Some(r)) => {
                        format!("{s:.1} s (baseline {bs:.1} s, ×{r:.2})")
                    }
                    (Some(s), _, _) => format!("{s:.1} s (no forced build or baseline)"),
                    _ => "no build report".into(),
                }
            }),
        },
    ]
}

fn hist_median(h: &[u32]) -> Option<usize> {
    let total: u64 = h.iter().map(|&c| u64::from(c)).sum();
    if total == 0 {
        return None;
    }
    let mut acc = 0;
    h.iter().position(|&c| {
        acc += u64::from(c);
        acc * 2 >= total
    })
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

fn tile_dir(t: [i32; 2]) -> String {
    format!("tiles/{}_{}", t[0], t[1])
}

/// Checks that flag tiles for review only (not a failure).
const REVIEW: [&str; 3] = ["roof", "color", "infra-review"];

const STYLE: &str = "<style>
:root{--bg:#fff;--fg:#1d1d1f;--mute:#666;--line:#ddd;--ok:#2e7d32;--bad:#c62828;--warn:#b26a00;--card:#f6f6f6}
@media (prefers-color-scheme: dark){:root{--bg:#161616;--fg:#eee;--mute:#aaa;--line:#333;--ok:#66bb6a;--bad:#ef5350;--warn:#ffb74d;--card:#222}}
body{background:var(--bg);color:var(--fg);font:15px/1.45 system-ui,sans-serif;margin:0 auto;max-width:1200px;padding:16px}
a{color:inherit}h1{font-size:1.5em}h2{font-size:1.2em;margin-top:2em;border-bottom:1px solid var(--line)}
table{border-collapse:collapse;width:100%}td,th{border-bottom:1px solid var(--line);padding:4px 8px;text-align:left;vertical-align:top}
.ok{color:var(--ok)}.bad{color:var(--bad)}.warn{color:var(--warn)}.mute{color:var(--mute)}
.map{display:grid;gap:3px}.map a{display:block;position:relative;border:3px solid var(--ok)}.map a.bad{border-color:var(--bad)}.map a.warn{border-color:var(--warn)}
.map img{display:block;width:100%;image-rendering:pixelated}.map span{position:absolute;left:2px;top:1px;font-size:11px;color:#fff;text-shadow:0 0 2px #000}
.imgs{display:grid;grid-template-columns:repeat(auto-fit,minmax(260px,1fr));gap:12px}.imgs figure{margin:0}.imgs img{width:100%;image-rendering:pixelated}
code{background:var(--card);padding:1px 4px;border-radius:3px;user-select:all}
</style>";

/// Writes out/qa/index.html and a page per tile.
pub fn write(qa: &Path, res: &[GroundQaResult], a: &AreaResults) -> Result<()> {
    let checks = checks(a);
    let flags = |t: &GroundQaResult| -> Vec<&'static str> {
        checks.iter().filter(|c| (c.flag)(t)).map(|c| c.title).collect()
    };
    let failing =
        |t: &GroundQaResult| checks.iter().any(|c| !REVIEW.contains(&c.id) && (c.flag)(t));
    let mut h = String::new();
    let flagged = res.iter().filter(|t| failing(t)).count();
    let _ = write!(
        h,
        "<!doctype html><html lang=en><meta charset=utf-8><meta name=viewport content='width=device-width,initial-scale=1'><title>QA report</title>{STYLE}\
         <h1>QA report: {}</h1><p class=mute>{} tiles, {} flagged by a check · QA ran in {:.0} s</p>",
        esc(a.area),
        res.len(),
        flagged,
        a.qa_secs
    );
    // Summary of the checks.
    h.push_str(
        "<h2>Checks</h2><table><tr><th>Check</th><th>Result</th><th>What it compares</th></tr>",
    );
    let row = |h: &mut String, name: &str, ok: Option<bool>, result: &str, what: &str| {
        let (cls, word) = match ok {
            Some(true) => ("ok", "pass"),
            Some(false) => ("bad", "fail"),
            None => ("mute", "review"),
        };
        let _ = write!(
            h,
            "<tr><td>{}</td><td><span class={cls}>{word}</span> {}</td><td class=mute>{}</td></tr>",
            esc(name),
            esc(result),
            esc(what)
        );
    };
    for c in &checks {
        let n = res.iter().filter(|t| (c.flag)(t)).count();
        let review = REVIEW.contains(&c.id);
        let ok = if review { None } else { Some(n == 0) };
        let msg = if review {
            format!("{n} tiles flagged for review")
        } else {
            format!("{n} of {} tiles flagged", res.len())
        };
        row(&mut h, c.title, ok, &msg, c.what);
    }
    row(
        &mut h,
        "Roof height (area)",
        Some(a.roof_median_m <= a.roof_median_max_m),
        &format!("median {:.2} m (limit {:.2} m)", a.roof_median_m, a.roof_median_max_m),
        "Median |highest building block − bDOM| over all building columns of the area.",
    );
    row(
        &mut h,
        "Roof color (area)",
        a.roof_de_judged.then(|| a.roof_de_median.is_none_or(|m| m as f64 <= a.roof_de_max)),
        &format!(
            "median ΔE2000 {:?} (limit {}){}",
            a.roof_de_median,
            a.roof_de_max,
            if a.roof_de_judged { "" } else { "; too few textured buildings to judge" }
        ),
        "Median ΔE2000 of textured roofs against DOP20 over the area.",
    );
    row(
        &mut h,
        "Building coverage (area)",
        Some(a.missing_buildings.is_empty()),
        &format!("{} LoD2 buildings produced no voxels", a.missing_buildings.len()),
        "Every LoD2 building, over all the tiles it was binned into.",
    );
    let det_bad = a.determinism.iter().filter(|d| d.2 > 0).count();
    row(
        &mut h,
        "Determinism",
        Some(det_bad == 0 && !a.determinism.is_empty()),
        &a.determinism
            .iter()
            .map(|(t, c, d)| format!("{},{}: {d} of {c} columns differ", t[0], t[1]))
            .collect::<Vec<_>>()
            .join(" · "),
        "Tiles rebuilt from scratch in memory against the region files on disk.",
    );
    match &a.server {
        Some((ok, tiles, asserts, when)) => row(
            &mut h,
            "Game load",
            Some(*ok),
            &format!("{tiles} tiles, {asserts} block assertions ({when})"),
            "Headless vanilla server loads every chunk; RCON block assertions (just server-test --area).",
        ),
        None => row(
            &mut h,
            "Game load",
            None,
            "not run for this world",
            "Run `just server-test --area <area>`.",
        ),
    }
    h.push_str("</table>");
    // Tile map.
    let (rx0, rx1) = (
        res.iter().map(|t| t.tile[0]).min().unwrap_or(0),
        res.iter().map(|t| t.tile[0]).max().unwrap_or(0),
    );
    let (rz0, rz1) = (
        res.iter().map(|t| t.tile[1]).min().unwrap_or(0),
        res.iter().map(|t| t.tile[1]).max().unwrap_or(0),
    );
    let _ = write!(
        h,
        "<h2>Tiles</h2><p class=mute>Green: no check flags the tile. Red: at least one does. Amber: flagged for review only. Click a tile for its page.</p>\
         <div class=map style='grid-template-columns:repeat({},1fr)'>",
        rx1 - rx0 + 1
    );
    let by: BTreeMap<[i32; 2], &GroundQaResult> = res.iter().map(|t| (t.tile, t)).collect();
    for rz in rz0..=rz1 {
        for rx in rx0..=rx1 {
            match by.get(&[rx, rz]) {
                Some(t) => {
                    let f = flags(t);
                    let _ = write!(
                        h,
                        "<a href='{d}/index.html' class='{}' title='{}'><img src='{d}/top.png' alt='tile {rx},{rz}' loading=lazy><span>{rx},{rz}</span></a>",
                        if failing(t) {
                            "bad"
                        } else if f.is_empty() {
                            ""
                        } else {
                            "warn"
                        },
                        esc(&if f.is_empty() { "no flags".to_string() } else { f.join(", ") }),
                        d = tile_dir([rx, rz])
                    );
                }
                None => h.push_str("<div></div>"),
            }
        }
    }
    h.push_str("</div>");
    // Worst tiles per check.
    h.push_str("<h2>Worst tiles per check</h2>");
    for c in &checks {
        let mut v: Vec<&GroundQaResult> = res.iter().filter(|t| (c.metric)(t) > 0.0).collect();
        v.sort_by(|x, y| (c.metric)(y).total_cmp(&(c.metric)(x)).then(x.tile.cmp(&y.tile)));
        let _ = write!(h, "<h3 id='{}'>{}</h3>", c.id, esc(c.title));
        if v.is_empty() {
            h.push_str("<p class=mute>Nothing on any tile.</p>");
            continue;
        }
        h.push_str("<table><tr><th>Tile</th><th>Flag</th><th>Value</th></tr>");
        for t in v.iter().take(20) {
            let _ = write!(
                h,
                "<tr><td><a href='{}/index.html'>{},{}</a></td><td>{}</td><td>{}</td></tr>",
                tile_dir(t.tile),
                t.tile[0],
                t.tile[1],
                if (c.flag)(t) { "<span class=bad>flagged</span>" } else { "" },
                esc(&(c.value)(t))
            );
        }
        h.push_str("</table>");
    }
    std::fs::write(qa.join("index.html"), h)?;
    for t in res {
        std::fs::write(
            qa.join(tile_dir(t.tile)).join("index.html"),
            tile_page(t, &checks, &flags(t)),
        )?;
    }
    Ok(())
}

fn tile_page(t: &GroundQaResult, checks: &[Check], flags: &[&str]) -> String {
    let key = TileKey::new(t.tile[0], t.tile[1]);
    let (bx, bz) = key.min_block();
    let half = TILE_SIZE / 2;
    let mut h = String::new();
    let _ = write!(
        h,
        "<!doctype html><html lang=en><meta charset=utf-8><meta name=viewport content='width=device-width,initial-scale=1'><title>Tile {x},{z}</title>{STYLE}\
         <p><a href='../../index.html'>← QA report</a></p><h1>Tile {x},{z}</h1>\
         <p class=mute>Region r.{x}.{z}.mca · blocks x {bx}…{}, z {bz}…{} · center <code>/tp @s {} 120 {}</code></p><p>{}</p>",
        bx + TILE_SIZE - 1,
        bz + TILE_SIZE - 1,
        bx + half,
        bz + half,
        if flags.is_empty() {
            "<span class=ok>No check flags this tile.</span>".to_string()
        } else {
            format!("<span class=bad>Flagged: {}</span>", esc(&flags.join(", ")))
        },
        x = t.tile[0],
        z = t.tile[1],
    );
    h.push_str("<div class=imgs>");
    for (f, cap) in [
        ("top.png", "World, top-down (palette colors, hillshade)"),
        ("dop.png", "DOP20 orthophoto"),
        (
            "ground.png",
            "Ground: green within tolerance of DGM1, red off; water blue, holes magenta; black not checked",
        ),
        (
            "surface.png",
            "Surface vs bDOM: gray within 2 m, red model higher, blue model lower; black not checked (water, trees)",
        ),
    ] {
        let _ = write!(
            h,
            "<figure><a href='{f}'><img src='{f}' alt='{}' loading=lazy></a><figcaption class=mute>{}</figcaption></figure>",
            esc(cap),
            esc(cap)
        );
    }
    h.push_str("</div><h2>Checks</h2><table>");
    for c in checks {
        let _ = write!(
            h,
            "<tr><td>{}</td><td>{}</td><td>{}</td></tr>",
            esc(c.title),
            if (c.flag)(t) { "<span class=bad>flagged</span>" } else { "" },
            esc(&(c.value)(t))
        );
    }
    h.push_str("</table><h2>Findings</h2>");
    let tp = |p: [i32; 3]| format!("<code>/tp @s {} {} {}</code>", p[0], p[1] + 2, p[2]);
    let mut items: Vec<String> = Vec::new();
    for p in &t.water_wall_samples {
        items.push(format!("Water wall {}", tp(*p)));
    }
    for p in &t.ground_hole_samples {
        items.push(format!("Ground hole {}", tp(*p)));
    }
    for p in &t.infra.samples {
        items.push(format!("Infrastructure {}", tp(*p)));
    }
    for (kind, p) in &t.infra.deck_findings {
        items.push(format!("Deck: {} <code>/tp @s {} ~ {}</code>", esc(kind), p[0], p[1]));
    }
    for p in &t.infra.mismatch_samples {
        items.push(format!("Lane mismatch {}", tp(*p)));
    }
    for id in &t.infra.empty_decks {
        items.push(format!("Deck over water drew nothing: {}", esc(id)));
    }
    for (id, gy, uy) in &t.buildings.floating {
        items.push(format!("Floating building {} (ground y {gy}, underside y {uy})", esc(id)));
    }
    for id in &t.buildings.empty {
        items.push(format!("Building produced no voxels: {}", esc(id)));
    }
    for id in &t.trees.trunks_missing {
        items.push(format!("Tree trunk missing: {}", esc(id)));
    }
    for (id, abs, signed, cols) in &t.buildings.roof_changed {
        items.push(format!(
            "Building changed since bDOM: {} (median |Δ| {abs:.1} m, model − bDOM {signed:.1} m, {cols} columns)",
            esc(id)
        ));
    }
    if items.is_empty() {
        h.push_str("<p class=mute>None.</p>");
    } else {
        h.push_str("<ul>");
        for i in items {
            let _ = write!(h, "<li>{i}</li>");
        }
        h.push_str("</ul>");
    }
    h.push_str("<p class=mute>Raw results: <a href='ground.json'>ground.json</a></p>");
    h
}
