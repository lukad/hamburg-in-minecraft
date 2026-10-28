//! Water barriers from OpenStreetMap: lock gates, locks, weirs, sluice gates and dams
//!. At city scale the seeded water bodies would otherwise flood through
//! every lock and weir into regulated canals, polder ditches and the reaches of streams
//! above their weirs. Each structure becomes [`Lock`] disks (a line or area is covered by
//! disks along it), used like the `[[lock]]` disks of `config/water.toml`.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use hhmc_core::geom::Bbox;
use hhmc_core::landcfg::{Lock, WaterConfig};
use serde_json::Value;
use tracing::info;

use crate::{Ctx, PREP_VERSION, gdal, key_of, mark, up_to_date};

/// The OSM extract's water structures as GeoJSON in EPSG:25832
/// (`data/prepared/water_structures/{points,lines,areas}.geojson`), rebuilt when the
/// extract changes.
pub fn extract(ctx: &Ctx) -> Result<Vec<PathBuf>> {
    let src = ctx.sources.get("osm").context("no source `osm`")?;
    let file = src.files.first().context("source `osm` has no file")?;
    let pbf = ctx.paths.raw().join("osm").join(&file.path);
    let dir = ctx.paths.prepared().join("water_structures");
    std::fs::create_dir_all(&dir)?;
    let sha = file.sha256.clone().unwrap_or_default();
    // OSM keys the driver puts into columns (`waterway` on lines) or into `other_tags`.
    let tag = |k: &str, v: &str| format!("other_tags LIKE '%\"{k}\"=>\"{v}\"%'");
    let kinds = ["lock_gate", "sluice_gate", "weir", "dam"];
    let in_tags: Vec<String> = kinds.iter().map(|k| tag("waterway", k)).collect();
    let queries = [
        (
            "points",
            format!("SELECT osm_id, name, other_tags FROM points WHERE {}", in_tags.join(" OR ")),
        ),
        (
            "lines",
            format!(
                "SELECT osm_id, name, waterway, other_tags FROM lines WHERE waterway IN ({}) OR {}",
                kinds.map(|k| format!("'{k}'")).join(", "),
                tag("lock", "yes")
            ),
        ),
        (
            "multipolygons",
            format!(
                "SELECT osm_id, name, other_tags FROM multipolygons WHERE {} OR {}",
                tag("lock", "yes"),
                in_tags.join(" OR ")
            ),
        ),
    ];
    let mut out = Vec::new();
    for (layer, sql) in queries {
        let path = dir.join(format!("{layer}.geojson"));
        let key = key_of(&[&PREP_VERSION.to_le_bytes(), sha.as_bytes(), sql.as_bytes()]);
        if !up_to_date(&path, &key) {
            let _ = std::fs::remove_file(&path);
            gdal::run(
                "ogr2ogr",
                [
                    "-f",
                    "GeoJSON",
                    "-t_srs",
                    "EPSG:25832",
                    "-sql",
                    &sql,
                    path.to_str().expect("utf-8 path"),
                    pbf.to_str().expect("utf-8 path"),
                ],
            )?;
            mark(&path, &key)?;
        }
        out.push(path);
    }
    Ok(out)
}

/// The kind of a structure: `waterway` (column or tag), or `lock` for `lock=yes`.
fn kind(props: &Value) -> Option<String> {
    let tags = props["other_tags"].as_str().unwrap_or("");
    if let Some(w) = props["waterway"].as_str()
        && !w.is_empty()
    {
        return Some(w.to_string());
    }
    if let Some(rest) = tags.split("\"waterway\"=>\"").nth(1) {
        return rest.split('"').next().map(str::to_string);
    }
    tags.contains("\"lock\"=>\"yes\"").then(|| "lock".to_string())
}

fn coords(v: &Value) -> Vec<(f64, f64)> {
    v.as_array()
        .map(|a| {
            a.iter().filter_map(|c| Some((c.get(0)?.as_f64()?, c.get(1)?.as_f64()?))).collect()
        })
        .unwrap_or_default()
}

/// Lines of a geometry (points as one-point lines, polygon rings as lines).
fn lines(g: &Value) -> Vec<Vec<(f64, f64)>> {
    let c = &g["coordinates"];
    match g["type"].as_str() {
        Some("Point") => {
            coords(&Value::Array(vec![c.clone()])).into_iter().map(|p| vec![p]).collect()
        }
        Some("LineString") => vec![coords(c)],
        Some("MultiLineString") | Some("Polygon") => {
            c.as_array().map(|a| a.iter().map(coords).collect()).unwrap_or_default()
        }
        Some("MultiPolygon") => c
            .as_array()
            .map(|ps| {
                ps.iter()
                    .flat_map(|p| p.as_array().cloned().unwrap_or_default())
                    .map(|r| coords(&r))
                    .collect()
            })
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}

/// Barrier disks of the structures in `files` that touch `bbox`, with the radius
/// `water.osm_barriers` gives their kind (kinds without one are ignored).
pub fn disks(files: &[PathBuf], water: &WaterConfig, bbox: &Bbox) -> Result<Vec<Lock>> {
    let mut out = Vec::new();
    for f in files {
        out.extend(disks_of(f, water, bbox)?);
    }
    info!(structures = out.len(), "OSM water barrier disks");
    Ok(out)
}

fn disks_of(path: &Path, water: &WaterConfig, bbox: &Bbox) -> Result<Vec<Lock>> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let doc: Value = serde_json::from_str(&text)?;
    let mut out = Vec::new();
    for f in doc["features"].as_array().into_iter().flatten() {
        let props = &f["properties"];
        let Some(k) = kind(props) else { continue };
        let Some(&r) = water.osm_barriers.get(&k) else { continue };
        let name = format!(
            "OSM {k} {}{}",
            props["osm_id"].as_str().unwrap_or("?"),
            props["name"].as_str().map(|n| format!(" ({n})")).unwrap_or_default()
        );
        for line in lines(&f["geometry"]) {
            // Disks every radius along the line cover a band at least 1.7 radii wide.
            let mut pts = Vec::new();
            for w in line.windows(2) {
                let (a, b) = (w[0], w[1]);
                let n = ((b.0 - a.0).hypot(b.1 - a.1) / r).ceil().max(1.0) as usize;
                pts.extend((0..n).map(|i| {
                    let t = i as f64 / n as f64;
                    (a.0 + (b.0 - a.0) * t, a.1 + (b.1 - a.1) * t)
                }));
            }
            pts.extend(line.last().copied());
            for (e, n) in pts {
                if bbox.buffered(r).contains(e, n) {
                    out.push(Lock { name: name.clone(), center: [e, n], radius_m: r });
                }
            }
        }
    }
    Ok(out)
}
