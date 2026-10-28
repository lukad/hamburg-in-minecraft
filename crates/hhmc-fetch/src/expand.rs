//! `hhmc sources expand`: lists the files of every source with a `tiled` rule that an
//! area needs, into `data/sources/<id>.toml`. Zip listings come from the archives'
//! central directories; nothing else is downloaded. Pins of entries that stay listed
//! are kept, and files already listed in sources.toml itself are left out.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

use anyhow::{Context, Result, bail};
use hhmc_core::Bbox;
use tracing::{info, warn};

use crate::remote_zip::list_remote_zip;
use crate::sources::{ExtraFiles, Source, SourceFile, Sources, Tiled};

/// Per source: number of files listed.
pub fn expand(sources_toml: &Path, area: &[Bbox], ids: &[String]) -> Result<Vec<(String, usize)>> {
    let sources = Sources::load(sources_toml)?;
    let mut out = Vec::new();
    for src in &sources.sources {
        let Some(rule) = &src.tiled else { continue };
        if !ids.is_empty() && !ids.contains(&src.id) {
            continue;
        }
        let files = list(src, rule, area).with_context(|| format!("source `{}`", src.id))?;
        let n = write(sources_toml, src, files)?;
        info!(source = %src.id, files = n, "expanded");
        out.push((src.id.clone(), n));
    }
    Ok(out)
}

fn needed(area: &[Bbox], b: &Bbox) -> bool {
    area.iter().any(|a| a.intersects(b))
}

fn basename(m: &str) -> &str {
    m.rsplit('/').next().unwrap_or(m)
}

/// The 1 km tile of a name `…_32_<EEE>_<NNNN>_1_…`, as a box.
fn utm32_km(name: &str) -> Option<Bbox> {
    let parts: Vec<&str> = name.split('_').collect();
    parts.windows(4).find_map(|w| {
        let digits = |s: &str, n| s.len() == n && s.bytes().all(|b| b.is_ascii_digit());
        if w[0] == "32" && digits(w[1], 3) && digits(w[2], 4) && w[3] == "1" {
            let e = w[1].parse::<f64>().ok()? * 1000.0;
            let n = w[2].parse::<f64>().ok()? * 1000.0;
            Some(Bbox::new(e, n, e + 1000.0, n + 1000.0))
        } else {
            None
        }
    })
}

fn list(src: &Source, rule: &Tiled, area: &[Bbox]) -> Result<Vec<SourceFile>> {
    let suffix = rule.suffix.as_deref().unwrap_or("");
    let file = |url: &str, path: String, member: Option<String>, bbox: Option<Bbox>| SourceFile {
        url: url.to_string(),
        member,
        member_prefix: None,
        path,
        sha256: None,
        size_bytes: None,
        bbox,
    };
    let mut files = Vec::new();
    match rule.kind.as_str() {
        "utm32_km" | "all" => {
            for a in &rule.archives {
                let members = list_remote_zip(&a.url)?;
                info!(url = %a.url, members = members.len(), "listed");
                for m in members {
                    let base = basename(&m.name);
                    if m.name.ends_with('/') || !base.ends_with(suffix) {
                        continue;
                    }
                    let bbox = if rule.kind == "all" {
                        None
                    } else {
                        let Some(b) = utm32_km(base) else { continue };
                        if !needed(area, &b) {
                            continue;
                        }
                        Some(b)
                    };
                    let path = format!("{}{base}", a.path_prefix);
                    files.push(file(&a.url, path, Some(m.name.clone()), bbox));
                }
            }
        }
        "lod3_folder" => {
            for a in &rule.archives {
                let members = list_remote_zip(&a.url)?;
                let folders: BTreeMap<String, ()> = members
                    .iter()
                    .filter_map(|m| m.name.split_once('/').map(|(d, _)| d))
                    .filter(|d| d.len() == 4 && d.bytes().all(|b| b.is_ascii_digit()))
                    .map(|d| (d.to_string(), ()))
                    .collect();
                info!(url = %a.url, folders = folders.len(), "listed");
                for d in folders.keys() {
                    let e = 500_000.0 + d[..2].parse::<f64>()? * 1000.0;
                    let n = 5_900_000.0 + d[2..].parse::<f64>()? * 1000.0;
                    let b = Bbox::new(e, n, e + 1000.0, n + 1000.0);
                    if needed(area, &b) {
                        let mut f = file(&a.url, format!("{}/{d}", a.path_prefix), None, Some(b));
                        f.member_prefix = Some(format!("{d}/"));
                        files.push(f);
                    }
                }
            }
        }
        "wfs_blocks" => {
            let tpl = rule.url_template.as_deref().context("wfs_blocks needs url_template")?;
            let s = rule.block_m.context("wfs_blocks needs block_m")?;
            let all = area.iter().copied().reduce(|a, b| a.union(&b)).context("empty area")?;
            let (i0, i1) = ((all.min_e / s).floor() as i64, (all.max_e / s).ceil() as i64);
            let (j0, j1) = ((all.min_n / s).floor() as i64, (all.max_n / s).ceil() as i64);
            for j in j0..j1 {
                for i in i0..i1 {
                    let b = Bbox::new(
                        i as f64 * s,
                        j as f64 * s,
                        (i + 1) as f64 * s,
                        (j + 1) as f64 * s,
                    );
                    if !needed(area, &b) {
                        continue;
                    }
                    let (x0, y0, x1, y1) =
                        (b.min_e as i64, b.min_n as i64, b.max_e as i64, b.max_n as i64);
                    let url = tpl
                        .replace("{minx}", &x0.to_string())
                        .replace("{miny}", &y0.to_string())
                        .replace("{maxx}", &x1.to_string())
                        .replace("{maxy}", &y1.to_string());
                    files.push(file(&url, format!("blocks/{x0}_{y0}.geojson"), None, Some(b)));
                }
            }
        }
        k => bail!("unknown tiled kind `{k}`"),
    }
    // Files sources.toml already lists stay there.
    let main: HashSet<&str> = src.files[..src.main_files].iter().map(|f| f.path.as_str()).collect();
    let mut seen = HashSet::new();
    files.retain(|f| {
        if main.contains(f.path.as_str()) {
            return false;
        }
        if !seen.insert(f.path.clone()) {
            warn!(path = %f.path, "listed twice; keeping the first");
            return false;
        }
        true
    });
    Ok(files)
}

/// Writes the list, keeping the pins of entries that are unchanged; returns its length.
fn write(sources_toml: &Path, src: &Source, mut files: Vec<SourceFile>) -> Result<usize> {
    let key =
        |f: &SourceFile| (f.url.clone(), f.member.clone(), f.member_prefix.clone(), f.path.clone());
    let old: HashMap<_, _> = src.files[src.main_files..]
        .iter()
        .map(|f| (key(f), (f.sha256.clone(), f.size_bytes)))
        .collect();
    for f in &mut files {
        if let Some((sha, size)) = old.get(&key(f)) {
            f.sha256.clone_from(sha);
            f.size_bytes = *size;
        }
    }
    let n = files.len();
    let path = Sources::extra_path(sources_toml, &src.id);
    std::fs::create_dir_all(path.parent().expect("has parent"))?;
    let body = toml::to_string(&ExtraFiles { files })?;
    std::fs::write(
        &path,
        format!(
            "# Files of source `{}` generated by `hhmc sources expand` from its `tiled` rule in\n\
             # data/sources.toml; sha256 and size_bytes are pinned by `hhmc fetch --pin`.\n\n{body}",
            src.id
        ),
    )?;
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn km_tiles_parse_from_names() {
        let b = utm32_km("dgm1_32_563_5931_1_hh_2022.tif").unwrap();
        assert_eq!((b.min_e, b.min_n, b.max_e), (563000.0, 5931000.0, 564000.0));
        assert!(utm32_km("dop20rgbi_32_570_5940_1_hh_2026_7.tif").is_some());
        assert!(utm32_km("LoD2_32_563_5931_1_HH.gml").is_some());
        assert!(utm32_km("dgm1_hh_2022-04-30.csv").is_none());
    }
}
