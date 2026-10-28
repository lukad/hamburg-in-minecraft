//! Per-tile normalization of the raster datasets.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use rayon::prelude::*;
use tracing::info;

use crate::{Ctx, PREP_VERSION, gdal, key_of, mark, up_to_date};

pub fn prepare(ctx: &Ctx, id: &str) -> Result<()> {
    let src = ctx.sources.get(id).ok_or_else(|| anyhow::anyhow!("no source `{id}`"))?;
    let raw = ctx.paths.raw().join(id);
    let out_dir = ctx.paths.prepared().join(id);
    std::fs::create_dir_all(&out_dir)?;

    // Inputs: the dataset's GeoTIFFs that fetch has verified.
    let mut inputs: Vec<(PathBuf, String, Option<hhmc_core::Bbox>)> = src
        .files
        .iter()
        .filter(|f| f.path.ends_with(".tif"))
        .filter_map(|f| {
            let p = raw.join(&f.path);
            p.exists().then(|| (p, f.sha256.clone().unwrap_or_default(), f.bbox))
        })
        .collect();
    inputs.sort_by(|a, b| a.0.cmp(&b.0));
    if inputs.is_empty() {
        bail!("no fetched GeoTIFFs for `{id}`; run `hhmc fetch` first");
    }

    // One VRT over all inputs; rebuilt when the input set changes.
    let vrt = out_dir.join("mosaic.vrt");
    let all: Vec<&[u8]> = inputs
        .iter()
        .flat_map(|(p, s, _)| [p.as_os_str().as_encoded_bytes(), s.as_bytes()])
        .collect();
    let vrt_key = key_of(&[&PREP_VERSION.to_le_bytes(), b"vrt", &all.concat()]);
    if !up_to_date(&vrt, &vrt_key) {
        let mut args: Vec<std::ffi::OsString> =
            vec!["-q".into(), "-overwrite".into(), vrt.clone().into()];
        args.extend(inputs.iter().map(|(p, _, _)| p.clone().into_os_string()));
        gdal::run("gdalbuildvrt", args)?;
        mark(&vrt, &vrt_key)?;
    }

    let origin = ctx.world.origin;
    let built: usize = ctx
        .tiles
        .par_iter()
        .map(|t| -> Result<usize> {
            let b = t.read_bbox(&origin);
            let out = out_dir.join(format!("{}_{}.tif", t.rx, t.rz));
            let used: Vec<&[u8]> = inputs
                .iter()
                .filter(|(_, _, bb)| bb.is_none_or(|bb| bb.intersects(&b)))
                .map(|(_, s, _)| s.as_bytes())
                .collect();
            let bbox = format!("{} {} {} {}", b.min_e, b.min_n, b.max_e, b.max_n);
            let key = key_of(&[
                &PREP_VERSION.to_le_bytes(),
                id.as_bytes(),
                bbox.as_bytes(),
                &used.concat(),
            ]);
            if up_to_date(&out, &key) {
                return Ok(0);
            }
            gdal::run(
                "gdalwarp",
                [
                    "-q",
                    "-overwrite",
                    "-t_srs",
                    "EPSG:25832",
                    "-te",
                    &b.min_e.to_string(),
                    &b.min_n.to_string(),
                    &b.max_e.to_string(),
                    &b.max_n.to_string(),
                    "-tr",
                    "1",
                    "1",
                    "-r",
                    "near",
                    "-ot",
                    "Float32",
                    "-dstnodata",
                    "-9999",
                    "-of",
                    "COG",
                    "-co",
                    "COMPRESS=DEFLATE",
                    "-co",
                    "OVERVIEWS=NONE",
                    vrt.to_str().expect("utf-8 path"),
                    out.to_str().expect("utf-8 path"),
                ],
            )
            .with_context(|| format!("{id}, tile {t}"))?;
            mark(&out, &key)?;
            Ok(1)
        })
        .sum::<Result<usize>>()?;
    info!(dataset = id, tiles = ctx.tiles.len(), rebuilt = built, "rasters prepared");
    Ok(())
}

/// DOP20 (20 cm RGBI, district parts with masks) → per-tile 1 m RGB + alpha COG by area
/// averaging (`data/prepared/dop20/<rx>_<rz>.tif`). gdalwarp honors each part's mask, so
/// the complementary parts of tiles on district borders mosaic correctly.
pub fn prepare_dop(ctx: &Ctx) -> Result<()> {
    let id = "dop20";
    let Some(src) = ctx.sources.get(id) else { return Ok(()) };
    let raw = ctx.paths.raw().join(id);
    let out_dir = ctx.paths.prepared().join(id);
    std::fs::create_dir_all(&out_dir)?;
    let inputs: Vec<(PathBuf, String, Option<hhmc_core::Bbox>)> = src
        .files
        .iter()
        .filter_map(|f| {
            let p = raw.join(&f.path);
            p.exists().then(|| (p, f.sha256.clone().unwrap_or_default(), f.bbox))
        })
        .collect();
    let origin = ctx.world.origin;
    let built: usize = ctx
        .tiles
        .par_iter()
        .map(|t| -> Result<usize> {
            let b = t.read_bbox(&origin);
            let used: Vec<&(PathBuf, String, Option<hhmc_core::Bbox>)> =
                inputs.iter().filter(|(_, _, bb)| bb.is_none_or(|bb| bb.intersects(&b))).collect();
            let out = out_dir.join(format!("{}_{}.tif", t.rx, t.rz));
            let shas: Vec<&[u8]> = used.iter().map(|(_, s, _)| s.as_bytes()).collect();
            let bbox = format!("{} {} {} {}", b.min_e, b.min_n, b.max_e, b.max_n);
            let key = key_of(&[
                &PREP_VERSION.to_le_bytes(),
                id.as_bytes(),
                bbox.as_bytes(),
                &shas.concat(),
            ]);
            if used.is_empty() || up_to_date(&out, &key) {
                return Ok(0);
            }
            let (minx, miny, maxx, maxy) = (
                b.min_e.to_string(),
                b.min_n.to_string(),
                b.max_e.to_string(),
                b.max_n.to_string(),
            );
            let mut args: Vec<String> = [
                "-q",
                "-overwrite",
                "-t_srs",
                "EPSG:25832",
                "-te",
                &minx,
                &miny,
                &maxx,
                &maxy,
                "-tr",
                "1",
                "1",
                "-r",
                "average",
                "-srcband",
                "1",
                "-srcband",
                "2",
                "-srcband",
                "3",
                "-dstalpha",
                "-of",
                "COG",
                "-co",
                "COMPRESS=DEFLATE",
                "-co",
                "OVERVIEWS=NONE",
            ]
            .iter()
            .map(|s| s.to_string())
            .collect();
            args.extend(used.iter().map(|(p, _, _)| p.to_string_lossy().into_owned()));
            args.push(out.to_string_lossy().into_owned());
            gdal::run("gdalwarp", &args)?;
            mark(&out, &key)?;
            Ok(1)
        })
        .sum::<Result<usize>>()?;
    info!(dataset = id, tiles = ctx.tiles.len(), rebuilt = built, "orthophotos prepared");
    Ok(())
}
