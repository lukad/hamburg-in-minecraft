//! `hhmc package`: the built world as a zip for players (or several, with a part size),
//! with `ATTRIBUTION.md` and a README with install steps. Entries carry a fixed timestamp
//! and are written in sorted order, so the same world gives the same zips.

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use hhmc_core::{Area, Areas, Paths, WorldConfig};
use tracing::info;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, DateTime};

/// World files a player needs; server and client state (players/, stats/, session.lock,
/// level.dat_old) stays out.
const WORLD_ENTRIES: [&str; 4] = ["level.dat", "data", "datapacks", "dimensions"];

/// Writes the world of `area` as one zip, or with `part_limit` as zips of at most that
/// many bytes each. The first part holds the README, ATTRIBUTION.md and everything but
/// the region files; the region files are spread over the parts in order. Unzipped into
/// the same folder, the parts make up one world folder.
pub fn package(
    paths: &Paths,
    area: &str,
    folder: &str,
    part_limit: Option<u64>,
) -> Result<Vec<PathBuf>> {
    let part_limit = part_limit.unwrap_or(u64::MAX);
    let world = WorldConfig::load(paths)?;
    let areas = Areas::load(paths)?;
    let a = areas.get(area)?;
    let src = paths.out().join("world");
    let layout = hhmc_world::WorldLayout::new(src.clone());
    let missing: Vec<String> = a
        .tiles
        .iter()
        .filter(|t| !layout.region_file(**t).exists())
        .map(|t| t.to_string())
        .collect();
    if !missing.is_empty() {
        bail!(
            "{} of {} tiles of `{area}` are not built (e.g. {}); run `hhmc build --area {area}`",
            missing.len(),
            a.tiles.len(),
            missing[0]
        );
    }
    let attribution = std::fs::read_to_string(paths.root.join("ATTRIBUTION.md"))
        .context("ATTRIBUTION.md missing; run `hhmc attribution`")?;

    let mut files = Vec::new();
    for e in WORLD_ENTRIES {
        collect(&src.join(e), &mut files)?;
    }
    files.sort();
    // Region files of other tiles (an earlier, larger build) do not belong to this area.
    let region_dir = layout.region_dir();
    let wanted: std::collections::HashSet<PathBuf> =
        a.tiles.iter().map(|t| layout.region_file(*t)).collect();
    files.retain(|f| f.parent() != Some(region_dir.as_path()) || wanted.contains(f));
    let size = |f: &Path| std::fs::metadata(f).map_or(0, |m| m.len());
    let world_bytes: u64 = files.iter().map(|f| size(f)).sum();

    // Parts by an upper bound of each entry's size in the zip: its bytes (region files are
    // stored, the rest deflated) plus local header, central directory record and zip64
    // fields for its name.
    let entry = |name_len: usize, bytes: u64| bytes + 2 * name_len as u64 + 200;
    let (rest, regions): (Vec<PathBuf>, Vec<PathBuf>) =
        files.into_iter().partition(|f| f.parent() != Some(region_dir.as_path()));
    let rel = |f: &Path| -> Result<String> {
        Ok(format!("{folder}/{}", f.strip_prefix(&src)?.to_string_lossy().replace('\\', "/")))
    };
    let mut parts: Vec<Vec<PathBuf>> = vec![rest.clone()];
    // Room for the README and ATTRIBUTION.md, which only the first part holds.
    let mut used = entry(64, attribution.len() as u64 + 8192);
    for f in &rest {
        used += entry(rel(f)?.len(), size(f));
    }
    ensure!(used < part_limit, "the world's non-region files alone exceed the part size");
    for f in regions {
        let e = entry(rel(&f)?.len(), size(&f));
        ensure!(e < part_limit, "{} is larger than the part size", f.display());
        if used + e > part_limit {
            parts.push(Vec::new());
            used = 0;
        }
        used += e;
        parts.last_mut().expect("one part").push(f);
    }
    let n = parts.len();

    let out_dir = paths.out().join("package");
    std::fs::create_dir_all(&out_dir)?;
    let stem = format!("{folder}-{area}");
    // Earlier packages of the area (one zip or another number of parts).
    for e in std::fs::read_dir(&out_dir)? {
        let p = e?.path();
        let name = p.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        if name == format!("{stem}.zip") || name.starts_with(&format!("{stem}.part")) {
            std::fs::remove_file(&p)?;
        }
    }
    let names: Vec<String> = (1..=n)
        .map(|i| if n == 1 { format!("{stem}.zip") } else { format!("{stem}.part{i}of{n}.zip") })
        .collect();
    let readme = readme(&world, area, a, world_bytes, folder, &names);
    let fixed = DateTime::default();
    let deflate = SimpleFileOptions::default()
        .compression_method(CompressionMethod::Deflated)
        .last_modified_time(fixed)
        .large_file(true);
    // Region files hold zlib-compressed chunks already.
    let stored = deflate.compression_method(CompressionMethod::Stored);
    let mut out = Vec::new();
    for (i, part) in parts.iter().enumerate() {
        let zip_path = out_dir.join(&names[i]);
        let tmp = zip_path.with_extension("zip.tmp");
        let mut zw = zip::ZipWriter::new(std::io::BufWriter::new(std::fs::File::create(&tmp)?));
        if i == 0 {
            zw.start_file(format!("{folder}/README.md"), deflate)?;
            zw.write_all(readme.as_bytes())?;
            zw.start_file(format!("{folder}/ATTRIBUTION.md"), deflate)?;
            zw.write_all(attribution.as_bytes())?;
        }
        for f in part {
            let name = rel(f)?;
            let opts = if name.ends_with(".mca") { stored } else { deflate };
            zw.start_file(name, opts)?;
            std::io::copy(&mut std::fs::File::open(f)?, &mut zw)?;
        }
        zw.finish()?.flush()?;
        std::fs::rename(&tmp, &zip_path)?;
        let zip_bytes = size(&zip_path);
        ensure!(zip_bytes <= part_limit, "{} exceeds the part size", zip_path.display());
        info!(zip = %zip_path.display(), files = part.len(), mb = zip_bytes >> 20, "package part written");
        out.push(zip_path);
    }
    info!(area, parts = n, world_mb = world_bytes >> 20, "package written");
    Ok(out)
}

fn collect(p: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    if p.is_dir() {
        for e in std::fs::read_dir(p)? {
            collect(&e?.path(), out)?;
        }
    } else if p.is_file() && p.extension().is_none_or(|x| x != "tmp") {
        out.push(p.to_path_buf());
    }
    Ok(())
}

fn readme(
    world: &WorldConfig,
    area: &str,
    a: &Area,
    bytes: u64,
    folder: &str,
    zips: &[String],
) -> String {
    let (description, tiles) = (&a.description, a.tiles.len());
    let (sx, sz) = world.origin.block_at(world.spawn.e, world.spawn.n);
    let (e0, n0) = (world.origin.e0, world.origin.n0);
    let version = &world.minecraft.version;
    let gb = bytes as f64 / 1e9;
    format!(
        "# Hamburg 1:1 in Minecraft

Hamburg built from open geodata at 1 block = 1 m: terrain, water, buildings (textured
LoD3 where available, LoD2 elsewhere), street and park trees, bridges and railways.

This archive holds the area `{area}`, {description}: {tiles} region files, {gb:.1} GB
unpacked.{cut} It is a plain vanilla world for Minecraft Java Edition {version} (data
version {dv}); no mods, resource packs or datapacks are needed. You spawn on the
Rathausmarkt at x {sx}, z {sz}. Hostile mobs, phantoms and mob griefing are off.

## Install

1. {unzip}
2. Move that folder into your Minecraft `saves` folder:
   - Official launcher on Linux: `~/.minecraft/saves/`
   - Official launcher on macOS: `~/Library/Application Support/minecraft/saves/`
   - Official launcher on Windows: `%APPDATA%\\.minecraft\\saves\\`
   - Prism Launcher and similar: the instance's `minecraft/saves/` (or `.minecraft/saves/`)
     folder, e.g. via *Folder* on the instance.
3. Start Minecraft {version} and pick the world in *Singleplayer*.

The world is large: give the game at least 4 GB of memory, and expect the first visit to a
district to take a moment while the game lights its chunks. Performance mods (Sodium,
Iris) and level-of-detail mods (Voxy, Distant Horizons) work with it as with any vanilla
world.

## Coordinates

One block is one meter. For a point at ETRS89 / UTM 32N (EPSG:25832) easting E and
northing N and height H (m above NHN):

- x = E − {e0}
- z = {n0} − N
- y = round(H); sea level is y = 0.

## Data and licenses

Built from open data of the Freie und Hansestadt Hamburg (LGV and others) and the federal
waterways administration (WSV). `ATTRIBUTION.md` lists every dataset with its publisher,
license and the attribution it requires; keep it with the world when you pass it on.

Not an official Minecraft product. Not approved by or associated with Mojang or Microsoft.
",
        dv = world.minecraft.data_version,
        unzip = if zips.len() == 1 {
            format!("Unzip this archive. It contains one folder, `{folder}/`.")
        } else {
            format!(
                "Download all {n} parts (`{first}` to `{last}`) into one folder and unzip \
                 them\n   there. Together they make one folder, `{folder}/`. The parts must \
                 be unzipped into the\n   same folder:\n\
                 \x20  - On macOS, double-clicking a zip makes a new folder for each part. \
                 Use Terminal in the\n     download folder instead:\n\
                 \x20    `for f in {stem}.part*.zip; do unzip -q \"$f\"; done`\n\
                 \x20  - On Windows, use *Extract All…* on each part and give every part the \
                 same destination\n     folder.\n\
                 \x20  - On Linux, `unzip` each part in the same folder.",
                n = zips.len(),
                first = zips[0],
                last = zips[zips.len() - 1],
                stem = zips[0].split(".part").next().unwrap_or(folder),
            )
        },
        cut = if a.boundary.is_some() {
            " The world ends at the city boundary; columns outside it are empty."
        } else {
            ""
        },
    )
}
