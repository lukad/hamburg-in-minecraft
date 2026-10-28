//! CityGML → per-tile building meshes, choosing LoD3 or LoD2 per building.
//!
//! LoD2-DE (2026) decides which buildings exist. A LoD2 building is replaced by LoD3.0-HH
//! geometry (2020, textured) when the LoD3 buildings assigned to it (each with ≥ 80 % of
//! its footprint inside the LoD2 one) cover ≥ 80 % of its footprint and the median per-column
//! roof height difference is at most `MAX_HEIGHT_DIFF`; otherwise the building is missing
//! from LoD3 or changed since
//! 2020 and LoD2 is used. Matching by id alone is not enough: most LoD3 ids are generated.
//! Footprints are compared as sets of block columns from the voxelizer.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::BufReader;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use hhmc_citygml::{BuildingMesh, parse_buildings};
use hhmc_core::raster::{Grid, NODATA, read_f32};
use hhmc_core::{Bbox, TileKey};
use rayon::prelude::*;
use serde::Serialize;
use tracing::info;

use crate::{Ctx, PREP_VERSION, covers, key_of, mark_covering};

const MIN_OVERLAP: f64 = 0.8;
/// LoD3 replaces a gated LoD2 building when its median |top − bDOM| over the LoD2
/// footprint is at least this many blocks smaller.
const BDOM_MARGIN: f64 = 1.0;

enum Gate {
    Footprint,
    Underside,
    Height,
}

/// A tile's prepared bDOM and DGM1.
type Dems = (Grid<f32>, Grid<f32>);

/// bDOM and DGM1 per block column, from the prepared tiles (a few kept in memory).
struct Surface<'a> {
    ctx: &'a Ctx,
    tiles: HashMap<TileKey, Option<Dems>>,
}

impl<'a> Surface<'a> {
    fn new(ctx: &'a Ctx) -> Self {
        Self { ctx, tiles: HashMap::new() }
    }

    /// (bDOM, DGM1) block heights at column (x, z), if both have data.
    fn at(&mut self, x: i32, z: i32) -> Result<Option<(i32, i32)>> {
        let t = TileKey::from_block(x, z);
        if !self.tiles.contains_key(&t) {
            if self.tiles.len() > 256 {
                self.tiles.clear();
            }
            let p = |id: &str| {
                self.ctx.paths.prepared().join(id).join(format!("{}_{}.tif", t.rx, t.rz))
            };
            let g = if p("bdom1").exists() && p("dgm1").exists() {
                Some((read_f32(&p("bdom1"))?, read_f32(&p("dgm1"))?))
            } else {
                None
            };
            self.tiles.insert(t, g);
        }
        let Some((b, d)) = &self.tiles[&t] else { return Ok(None) };
        let (e, n) = self.ctx.world.origin.block_center(x, z);
        let y = |v: f32| self.ctx.world.height.to_y(f64::from(v)).y;
        Ok(match (b.at(e, n), d.at(e, n)) {
            (Some(bv), Some(dv)) if bv != NODATA && dv != NODATA => Some((y(bv), y(dv))),
            _ => None,
        })
    }

    /// Whether every column of the LoD3 group starts more than [`MAX_HEIGHT_DIFF`] above
    /// the DGM1 ground while some LoD2 column starts within it.
    fn lod3_floats<'b>(
        &mut self,
        foot2: &HashMap<(i32, i32), (i32, i32)>,
        group: impl Iterator<Item = &'b HashMap<(i32, i32), (i32, i32)>>,
    ) -> Result<bool> {
        let lift = |s: &mut Self, (x, z): (i32, i32), bot: i32| -> Result<Option<i32>> {
            Ok(s.at(x, z)?.map(|(_, ground)| bot - ground))
        };
        let mut lod3_min = i32::MAX;
        let mut cols: Vec<((i32, i32), i32)> =
            group.flat_map(|f| f.iter().map(|(c, v)| (*c, v.0))).collect();
        cols.sort_unstable();
        for (c, bot) in cols {
            if let Some(l) = lift(self, c, bot)? {
                lod3_min = lod3_min.min(l);
            }
        }
        let mut lod2_min = i32::MAX;
        let mut cols2: Vec<&(i32, i32)> = foot2.keys().collect();
        cols2.sort_unstable();
        for c in cols2 {
            if let Some(l) = lift(self, *c, foot2[c].0)? {
                lod2_min = lod2_min.min(l);
            }
        }
        let limit = MAX_HEIGHT_DIFF as i32;
        Ok(lod3_min != i32::MAX && lod3_min > limit && lod2_min <= limit)
    }

    /// Whether the LoD3 group explains bDOM over the LoD2 footprint clearly better than
    /// LoD2 (where the group has no column, its surface is the DGM1 ground).
    fn lod3_fits_better<'b>(
        &mut self,
        foot2: &HashMap<(i32, i32), (i32, i32)>,
        group: impl Iterator<Item = &'b HashMap<(i32, i32), (i32, i32)>> + Clone,
    ) -> Result<bool> {
        let mut cols: Vec<&(i32, i32)> = foot2.keys().collect();
        cols.sort();
        let (mut e2, mut e3) = (Vec::new(), Vec::new());
        for c in cols {
            let Some((b, ground)) = self.at(c.0, c.1)? else { continue };
            let t2 = foot2[c].1;
            let t3 = group.clone().filter_map(|f| f.get(c)).map(|v| v.1).max().unwrap_or(ground);
            e2.push((t2 - b).abs());
            e3.push((t3 - b).abs());
        }
        if e2.len() * 2 < foot2.len() {
            return Ok(false);
        }
        e2.sort_unstable();
        e3.sort_unstable();
        let (m2, m3) = (e2[e2.len() / 2], e3[e3.len() / 2]);
        Ok(f64::from(m3) + BDOM_MARGIN <= f64::from(m2))
    }
}

/// ALKIS `AX_LageZurErdoberflaeche_Gebaeude` code "Unter der Erdoberfläche"
/// (https://registry.gdi-de.org/codelist/de.adv-online.gid/AX_LageZurErdoberflaeche_Gebaeude/1200).
const ALKIS_UNDERGROUND: &str = "1200";

/// Ids of `AX_Gebaeude` records that ALKIS places below the ground surface. LoD2 sometimes
/// models such buildings (e.g. the underground Mojo Club, DEHHALKA10007wzE) as tall blocks.
fn alkis_underground(files: &[PathBuf]) -> Result<HashSet<String>> {
    use quick_xml::events::Event;
    let per_file: Vec<Vec<String>> = files
        .par_iter()
        .map(|p| -> Result<Vec<String>> {
            let mut r = quick_xml::Reader::from_reader(BufReader::new(std::fs::File::open(p)?));
            let mut buf = Vec::new();
            let (mut id, mut in_lage, mut out) = (None::<String>, false, Vec::new());
            loop {
                match r.read_event_into(&mut buf).with_context(|| format!("NAS {}", p.display()))? {
                    Event::Start(e) => match e.local_name().as_ref() {
                        "AX_Gebaeude" => {
                            id = e
                                .attributes()
                                .flatten()
                                .find(|a| a.key.local_name().as_ref() == "id")
                                .map(|a| a.value.to_string());
                        }
                        "lageZurErdoberflaeche" if id.is_some() => in_lage = true,
                        _ => {}
                    },
                    Event::Text(t) if in_lage => {
                        if t.as_ref().trim() == ALKIS_UNDERGROUND
                            && let Some(i) = &id
                        {
                            out.push(i.clone());
                        }
                    }
                    Event::End(e) => match e.local_name().as_ref() {
                        "lageZurErdoberflaeche" => in_lage = false,
                        "AX_Gebaeude" => id = None,
                        _ => {}
                    },
                    Event::Eof => break,
                    _ => {}
                }
                buf.clear();
            }
            Ok(out)
        })
        .collect::<Result<_>>()?;
    Ok(per_file.into_iter().flatten().collect())
}
const MAX_HEIGHT_DIFF: f64 = 3.0;

fn parse_files(files: &[PathBuf], source: &str, root: &Path) -> Result<Vec<BuildingMesh>> {
    let per_file: Vec<Vec<BuildingMesh>> = files
        .par_iter()
        .map(|p| -> Result<Vec<BuildingMesh>> {
            let dir = p.parent().expect("file in a directory").to_path_buf();
            let resolve = |uri: &str| {
                let full = dir.join(uri.replace('\\', "/"));
                full.strip_prefix(root)
                    .map(|r| r.to_string_lossy().into_owned())
                    .unwrap_or_else(|_| full.to_string_lossy().into_owned())
            };
            let mut v = Vec::new();
            parse_buildings(BufReader::new(std::fs::File::open(p)?), |b| {
                if let Some(m) = BuildingMesh::from_building_with(&b, source, &resolve) {
                    v.push(m);
                }
            })
            .with_context(|| format!("parsing {}", p.display()))?;
            Ok(v)
        })
        .collect::<Result<_>>()?;
    let mut seen = HashSet::new();
    Ok(per_file.into_iter().flatten().filter(|m| seen.insert(m.id.clone())).collect())
}

/// Every `.gml` / `.xml` below `dir`, sorted.
fn gml_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().is_some_and(|x| x == "gml" || x == "xml") {
                out.push(p);
            }
        }
    }
    out.sort();
    out
}

#[derive(Serialize, Default)]
struct MatchReport {
    lod2: usize,
    lod3: usize,
    lod2_replaced_by_lod3: usize,
    lod3_used: usize,
    lod2_kept_no_lod3: usize,
    lod2_kept_footprint: usize,
    lod2_kept_height: usize,
    lod2_kept_underside: usize,
    /// LoD2 buildings a gate would have kept that LoD3 replaces because it fits bDOM
    /// clearly better.
    lod3_by_bdom: usize,
    /// LoD2 kept because its LoD3 group floats above the ground everywhere.
    lod2_kept_floating: usize,
    lod3_dropped_unmatched: usize,
    /// Median per-column top difference (LoD3 − LoD2, blocks) of buildings kept for height.
    height_rejected_diffs: Vec<f32>,
}

/// Footprint columns with the (lowest underside, highest top) block y of each.
fn columns(
    m: &BuildingMesh,
    origin: &hhmc_core::config::Origin,
) -> HashMap<(i32, i32), (i32, i32)> {
    hhmc_buildings::voxelize(m, origin)
        .columns
        .into_iter()
        .map(|(k, iv)| {
            let bot = iv.iter().map(|i| i.bot).min().unwrap_or(i32::MIN);
            (k, (bot, iv.iter().map(|i| i.top).max().unwrap_or(i32::MIN)))
        })
        .collect()
}

pub fn prepare(ctx: &Ctx) -> Result<()> {
    let root = ctx.paths.root.clone();
    let lod2_files = gml_files(&ctx.paths.raw().join("lod2"));
    let lod3_files = gml_files(&ctx.paths.raw().join("lod3"));
    let mut key_parts: Vec<String> = Vec::new();
    for id in ["lod2", "lod3", "alkis_nas"] {
        if let Some(s) = ctx.sources.get(id) {
            key_parts.extend(s.files.iter().map(|f| f.sha256.clone().unwrap_or_default()));
        }
    }
    // The bDOM / DGM1 tiles decide gated matches, by their mosaics' keys.
    for id in ["bdom1", "dgm1"] {
        let k = ctx.paths.prepared().join(id).join("mosaic.vrt.key");
        key_parts
            .push(std::fs::read_to_string(&k).with_context(|| format!("reading {}", k.display()))?);
    }
    let ver = hhmc_buildings::BUILDINGS_VERSION.to_le_bytes();
    let key =
        key_of(&[&PREP_VERSION.to_le_bytes(), &ver, b"buildings", key_parts.join(";").as_bytes()]);
    let out_dir = ctx.paths.prepared().join("buildings");
    let index_path = out_dir.join("index.json");
    if covers(&index_path, &key, &ctx.tiles) {
        return Ok(());
    }
    let origin = ctx.world.origin;
    let mut nas_files: Vec<PathBuf> = std::fs::read_dir(ctx.paths.raw().join("alkis_nas"))
        .map(|rd| {
            rd.flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|x| x == "xml"))
                .collect()
        })
        .unwrap_or_default();
    nas_files.sort();
    let underground = alkis_underground(&nas_files)?;
    let lod2: Vec<BuildingMesh> = parse_files(&lod2_files, "lod2", &root)?;
    let lod2_underground = lod2.iter().filter(|m| underground.contains(&m.id)).count();
    let lod2: Vec<BuildingMesh> =
        lod2.into_iter().filter(|m| !underground.contains(&m.id)).collect();
    let lod3: Vec<BuildingMesh> = parse_files(&lod3_files, "lod3", &root)?
        .into_iter()
        .filter(|m| !underground.contains(&m.id))
        .collect();
    info!(
        alkis_underground = underground.len(),
        lod2_dropped = lod2_underground,
        "buildings ALKIS places below ground"
    );

    // Footprints and heights, in parallel.
    let foot2: Vec<HashMap<(i32, i32), (i32, i32)>> =
        lod2.par_iter().map(|m| columns(m, &origin)).collect();
    let foot3: Vec<HashMap<(i32, i32), (i32, i32)>> =
        lod3.par_iter().map(|m| columns(m, &origin)).collect();
    let mut owner2: HashMap<(i32, i32), usize> = HashMap::new();
    for (i, f) in foot2.iter().enumerate() {
        for &c in f.keys() {
            owner2.insert(c, i);
        }
    }
    // Assign each LoD3 building to the LoD2 building holding most of its footprint.
    let mut assigned: Vec<Vec<usize>> = vec![Vec::new(); lod2.len()];
    let mut report = MatchReport { lod2: lod2.len(), lod3: lod3.len(), ..Default::default() };
    for (j, f) in foot3.iter().enumerate() {
        let mut count: HashMap<usize, usize> = HashMap::new();
        for c in f.keys() {
            if let Some(&i) = owner2.get(c) {
                *count.entry(i).or_default() += 1;
            }
        }
        match count.into_iter().max_by_key(|&(i, n)| (n, std::cmp::Reverse(i))) {
            Some((i, n)) if !f.is_empty() && n as f64 >= MIN_OVERLAP * f.len() as f64 => {
                assigned[i].push(j)
            }
            _ => report.lod3_dropped_unmatched += 1,
        }
    }
    let mut surface = Surface::new(ctx);
    let mut chosen: Vec<std::borrow::Cow<BuildingMesh>> = Vec::new();
    for (i, m2) in lod2.iter().enumerate() {
        let group = &assigned[i];
        if group.is_empty() {
            report.lod2_kept_no_lod3 += 1;
            chosen.push(std::borrow::Cow::Borrowed(m2));
            continue;
        }
        // Per covered column: top of the LoD3 group − top of LoD2, and how far the group's
        // underside lies above the LoD2 one.
        let mut diffs: Vec<i32> = Vec::new();
        let mut lifts: Vec<i32> = Vec::new();
        for (c, &(b2, t2)) in &foot2[i] {
            let tops = group.iter().filter_map(|&j| foot3[j].get(c));
            let (mut b3, mut t3, mut any) = (i32::MAX, i32::MIN, false);
            for &(b, t) in tops {
                b3 = b3.min(b);
                t3 = t3.max(t);
                any = true;
            }
            if any {
                diffs.push(t3 - t2);
                if b2 != i32::MIN && b3 != i32::MIN {
                    lifts.push(b3 - b2);
                }
            }
        }
        let covered = diffs.len();
        lifts.sort_unstable();
        diffs.sort_by_key(|d| d.abs());
        let median = diffs.get(diffs.len() / 2).copied().unwrap_or(0);
        // Gates that keep LoD2: LoD3 covers too little of it, only models upper stories
        // or overhangs (underside far above LoD2's), or is off in height (often rebuilt
        // since 2020). For the first, bDOM decides: LoD3 is used anyway when it explains
        // bDOM over the LoD2 footprint clearly better (towers: LoD2 extrudes the site).
        let gate = if (covered as f64) < MIN_OVERLAP * foot2[i].len() as f64 {
            Some(Gate::Footprint)
        } else if lifts.get(lifts.len() / 2).is_some_and(|&l| f64::from(l) > MAX_HEIGHT_DIFF) {
            Some(Gate::Underside)
        } else if f64::from(median.abs()) > MAX_HEIGHT_DIFF {
            Some(Gate::Height)
        } else {
            None
        };
        if let Some(g) = gate {
            // Not for off heights, which stay with the newer LoD2: bDOM (2022) predates
            // rebuilds that LoD2 (2026) has. The other two gates are about geometry: is
            // what LoD3 leaves out ground or building (LoD2 too coarse, as at towers and
            // their platforms, or LoD3 incomplete)?
            // Under the underside gate LoD3 must also stand on the ground somewhere (a
            // tower's shaft); pieces that only model upper stories would float.
            let grounded = lifts.first().is_some_and(|&l| f64::from(l) <= MAX_HEIGHT_DIFF);
            let eligible = match g {
                Gate::Footprint => true,
                Gate::Underside => grounded,
                Gate::Height => false,
            };
            if eligible && surface.lod3_fits_better(&foot2[i], group.iter().map(|&j| &foot3[j]))? {
                report.lod3_by_bdom += 1;
            } else {
                match g {
                    Gate::Footprint => report.lod2_kept_footprint += 1,
                    Gate::Underside => report.lod2_kept_underside += 1,
                    Gate::Height => {
                        report.lod2_kept_height += 1;
                        report.height_rejected_diffs.push(median as f32);
                    }
                }
                report.lod3_dropped_unmatched += group.len();
                chosen.push(std::borrow::Cow::Borrowed(m2));
                continue;
            }
        }
        // LoD3 that floats above the ground everywhere while LoD2 stands on it is placed
        // too high (a few LoD3 models sit meters above DGM1): LoD2 stays.
        if surface.lod3_floats(&foot2[i], group.iter().map(|&j| &foot3[j]))? {
            report.lod2_kept_floating += 1;
            report.lod3_dropped_unmatched += group.len();
            chosen.push(std::borrow::Cow::Borrowed(m2));
            continue;
        }
        report.lod2_replaced_by_lod3 += 1;
        report.lod3_used += group.len();
        // One building under the LoD2 id, with the LoD3 pieces as its parts.
        let members: Vec<&BuildingMesh> = group.iter().map(|&j| &lod3[j]).collect();
        let mut merged = BuildingMesh::merge(&m2.id, &members);
        // LoD3 carries no function; the LoD2 building's (ALKIS object type and function)
        // applies to the merged building.
        if merged.function.is_none() {
            merged.function = m2.function.clone();
        }
        chosen.push(std::borrow::Cow::Owned(merged));
    }

    let mut bins: BTreeMap<TileKey, Vec<&BuildingMesh>> =
        ctx.tiles.iter().map(|&t| (t, Vec::new())).collect();
    let mut index: BTreeMap<&str, (&str, Vec<[i32; 2]>)> = BTreeMap::new();
    for m in chosen.iter().map(|c| c.as_ref()) {
        let b = Bbox::new(m.bbox[0], m.bbox[1], m.bbox[3], m.bbox[4]).buffered(1.0);
        // The area's tiles among those the box touches.
        for t in hhmc_core::tile::tiles_covering(&origin, &b) {
            if b.intersects(&t.bbox(&origin))
                && let Some(bin) = bins.get_mut(&t)
            {
                bin.push(m);
                index.entry(&m.id).or_insert((&m.source, Vec::new())).1.push([t.rx, t.rz]);
            }
        }
    }
    std::fs::create_dir_all(&out_dir)?;
    bins.par_iter().try_for_each(|(t, ms)| -> Result<()> {
        let raw = postcard::to_stdvec(ms)?;
        std::fs::write(
            out_dir.join(format!("{}_{}.bin", t.rx, t.rz)),
            zstd::encode_all(&raw[..], 3)?,
        )?;
        Ok(())
    })?;
    std::fs::write(out_dir.join("match.json"), serde_json::to_string_pretty(&report)? + "\n")?;
    std::fs::write(&index_path, serde_json::to_string(&index)? + "\n")?;
    mark_covering(&index_path, &key, &ctx.tiles)?;
    info!(
        lod2 = report.lod2,
        lod3 = report.lod3,
        replaced = report.lod2_replaced_by_lod3,
        lod3_used = report.lod3_used,
        kept_no_lod3 = report.lod2_kept_no_lod3,
        kept_footprint = report.lod2_kept_footprint,
        kept_height = report.lod2_kept_height,
        kept_underside = report.lod2_kept_underside,
        lod3_by_bdom = report.lod3_by_bdom,
        kept_floating = report.lod2_kept_floating,
        lod3_dropped = report.lod3_dropped_unmatched,
        in_area = index.len(),
        "building meshes prepared"
    );
    Ok(())
}
