//! Resumable, verifying downloader.

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use hhmc_core::Bbox;
use sha2::{Digest, Sha256};
use tracing::{info, warn};

use crate::remote_zip::{HttpRangeReader, with_retry};
use crate::sources::{SourceFile, Sources};

type RemoteArchive = zip::ZipArchive<HttpRangeReader>;
/// New pins are written after this many files.
const PIN_EVERY: usize = 50;

const USER_AGENT: &str =
    concat!("hhmc/", env!("CARGO_PKG_VERSION"), " (hamburg-mc open-data pipeline)");

/// Ask before anything this large is downloaded in one go.
pub const LARGE_DOWNLOAD_BYTES: u64 = 5_000_000_000;

#[derive(Clone, Debug, Default)]
pub struct FetchOptions {
    /// Only these source ids; all if empty.
    pub datasets: Vec<String>,
    /// Only files intersecting this box (the area's read box); all if `None`.
    pub bbox: Option<Bbox>,
    /// Record sha256 and size of unpinned files in `sources.toml` instead of failing.
    pub pin: bool,
    /// Accept and record new checksums for files whose content changed upstream.
    pub repin: bool,
    /// Permit single files larger than [`LARGE_DOWNLOAD_BYTES`].
    pub allow_large: bool,
    /// Only report what would be downloaded.
    pub dry_run: bool,
}

#[derive(Debug, Default)]
pub struct FetchReport {
    pub verified: usize,
    pub downloaded: usize,
    pub pinned: usize,
    pub skipped_area: usize,
    pub bytes_downloaded: u64,
}

/// Fetches all selected files of `sources_toml` into `raw_dir/<id>/<path>`.
pub fn fetch(sources_toml: &Path, raw_dir: &Path, opts: &FetchOptions) -> Result<FetchReport> {
    let sources = Sources::load(sources_toml)?;
    for id in &opts.datasets {
        if sources.get(id).is_none() {
            bail!("unknown dataset `{id}` in {}", sources_toml.display());
        }
    }
    let agent = agent();
    let mut report = FetchReport::default();
    let mut pins: Vec<(usize, usize, String, u64)> = Vec::new();
    let mut archives: HashMap<String, RemoteArchive> = HashMap::new();

    for (si, src) in sources.sources.iter().enumerate() {
        if !opts.datasets.is_empty() && !opts.datasets.contains(&src.id) {
            continue;
        }
        for (fi, file) in src.files.iter().enumerate() {
            if !file.needed_for(opts.bbox.as_ref()) {
                report.skipped_area += 1;
                continue;
            }
            let dest = raw_dir.join(&src.id).join(&file.path);
            let is_dir = file.member_prefix.is_some();
            match &file.sha256 {
                Some(expected) if is_verified(&dest, expected, is_dir)? => {
                    report.verified += 1;
                    continue;
                }
                None if !opts.pin && !opts.dry_run => bail!(
                    "{}: {} has no sha256 in sources.toml; run `hhmc fetch --pin --dataset {}` once and commit the result",
                    src.id,
                    file.path,
                    src.id
                ),
                _ => {}
            }
            if let Some(size) = file.size_bytes
                && size > LARGE_DOWNLOAD_BYTES
                && !opts.allow_large
            {
                bail!(
                    "{}: {} is {:.1} GB; pass --allow-large to download it",
                    src.id,
                    file.path,
                    size as f64 / 1e9
                );
            }
            if opts.dry_run {
                info!(dataset = %src.id, url = %file.url, size = ?file.size_bytes, "would download");
                continue;
            }
            let (sha, size) = if file.member.is_some() || file.member_prefix.is_some() {
                let archive = match archives.entry(file.url.clone()) {
                    Entry::Occupied(e) => e.into_mut(),
                    Entry::Vacant(e) => {
                        info!(url = %file.url, "reading remote zip directory");
                        let r = HttpRangeReader::open(&agent, &file.url)?;
                        e.insert(
                            zip::ZipArchive::new(r).with_context(|| format!("zip {}", file.url))?,
                        )
                    }
                };
                match (&file.member, &file.member_prefix) {
                    (Some(member), _) => extract_member(archive, &file.url, member, &dest)?,
                    (_, Some(prefix)) => extract_prefix(archive, &file.url, prefix, &dest)?,
                    _ => unreachable!(),
                }
            } else {
                download(&agent, file, &dest, opts.allow_large)?
            };
            report.downloaded += 1;
            report.bytes_downloaded += size;
            match &file.sha256 {
                Some(expected) if !expected.eq_ignore_ascii_case(&sha) => {
                    if opts.repin {
                        warn!(dataset = %src.id, path = %file.path, "content changed upstream; re-pinning");
                        pins.push((si, fi, sha.clone(), size));
                    } else {
                        let mut bad = dest.as_os_str().to_owned();
                        bad.push(".mismatch");
                        let bad = PathBuf::from(bad);
                        let _ = fs::remove_dir_all(&bad).or_else(|_| fs::remove_file(&bad));
                        fs::rename(&dest, &bad)?;
                        let hint = if src.volatile {
                            format!(
                                "this is a web-service source whose data changes upstream; review and run `hhmc fetch --repin --dataset {}`",
                                src.id
                            )
                        } else {
                            "the download is corrupt or the publisher replaced the file".to_owned()
                        };
                        bail!(
                            "{}: {} sha256 mismatch: expected {expected}, got {sha} (kept as {}); {hint}",
                            src.id,
                            file.path,
                            bad.display()
                        );
                    }
                }
                Some(_) => {}
                None => pins.push((si, fi, sha.clone(), size)),
            }
            write_marker(&dest, &sha, is_dir)?;
            // Pin as we go: a long run that stops keeps what it verified.
            if pins.len() >= PIN_EVERY {
                report.pinned += pins.len();
                pin(sources_toml, &sources, &pins)?;
                pins.clear();
            }
        }
    }
    if !pins.is_empty() {
        report.pinned += pins.len();
        pin(sources_toml, &sources, &pins)?;
    }
    Ok(report)
}

pub(crate) fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .user_agent(USER_AGENT)
        .timeout_connect(Some(Duration::from_secs(30)))
        .timeout_recv_response(Some(Duration::from_secs(120)))
        .build()
        .into()
}

fn marker_path(dest: &Path) -> PathBuf {
    let mut s = dest.as_os_str().to_owned();
    s.push(".verified");
    PathBuf::from(s)
}

/// Sidecar `<path>.verified` holding `sha256 size mtime` for a file, or `sha256 size count`
/// for an extracted directory, so verified data is not rehashed on every fetch.
fn write_marker(dest: &Path, sha: &str, is_dir: bool) -> Result<()> {
    let (a, b) = stamp(dest, is_dir)?;
    fs::write(marker_path(dest), format!("{sha} {a} {b}\n"))?;
    Ok(())
}

fn stamp(dest: &Path, is_dir: bool) -> Result<(u64, u64)> {
    if is_dir {
        let files = list_files(dest)?;
        let total =
            files.iter().map(|(_, p)| fs::metadata(p).map(|m| m.len())).sum::<io::Result<u64>>()?;
        Ok((total, files.len() as u64))
    } else {
        let meta = fs::metadata(dest)?;
        Ok((meta.len(), meta.modified()?.duration_since(UNIX_EPOCH)?.as_secs()))
    }
}

fn is_verified(dest: &Path, expected: &str, is_dir: bool) -> Result<bool> {
    if !dest.exists() {
        return Ok(false);
    }
    let (a, b) = stamp(dest, is_dir)?;
    if let Ok(m) = fs::read_to_string(marker_path(dest)) {
        let f: Vec<&str> = m.split_whitespace().collect();
        if f.len() == 3
            && f[0].eq_ignore_ascii_case(expected)
            && f[1] == a.to_string()
            && f[2] == b.to_string()
        {
            return Ok(true);
        }
    }
    // No or stale marker: hash once.
    let (sha, _) = if is_dir { hash_dir(dest)? } else { (hash_file(dest)?, 0) };
    if sha.eq_ignore_ascii_case(expected) {
        write_marker(dest, &sha, is_dir)?;
        Ok(true)
    } else {
        warn!(path = %dest.display(), "existing data does not match sha256; fetching again");
        Ok(false)
    }
}

/// All regular files below `dir` as (relative path with `/`, absolute path), sorted.
fn list_files(dir: &Path) -> Result<Vec<(String, PathBuf)>> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in fs::read_dir(&d)? {
            let e = e?;
            let p = e.path();
            if e.file_type()?.is_dir() {
                stack.push(p);
            } else {
                let rel = p
                    .strip_prefix(dir)?
                    .components()
                    .map(|c| c.as_os_str().to_string_lossy())
                    .collect::<Vec<_>>()
                    .join("/");
                out.push((rel, p));
            }
        }
    }
    out.sort();
    Ok(out)
}

/// Digest of a directory: sha256 over sorted `"<sha256>  <relative path>\n"` lines.
fn hash_dir(dir: &Path) -> Result<(String, u64)> {
    let mut h = Sha256::new();
    let mut total = 0;
    for (rel, p) in list_files(dir)? {
        total += fs::metadata(&p)?.len();
        h.update(format!("{}  {rel}\n", hash_file(&p)?).as_bytes());
    }
    Ok((hex::encode(h.finalize()), total))
}

fn hash_file(path: &Path) -> Result<String> {
    let mut h = Sha256::new();
    let mut r = BufReader::with_capacity(1 << 20, File::open(path)?);
    io::copy(&mut r, &mut HashWriter(&mut h))?;
    Ok(hex::encode(h.finalize()))
}

struct HashWriter<'a>(&'a mut Sha256);

impl Write for HashWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.update(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Downloads `file.url` to `dest` via `<dest>.part`, resuming a partial download with
/// an HTTP range request when the server supports it. Returns (sha256, size).
fn download(
    agent: &ureq::Agent,
    file: &SourceFile,
    dest: &Path,
    allow_large: bool,
) -> Result<(String, u64)> {
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut part = dest.as_os_str().to_owned();
    part.push(".part");
    let part = PathBuf::from(part);

    let mut hasher = Sha256::new();
    let mut have = 0u64;
    if let Ok(meta) = fs::metadata(&part) {
        have = meta.len();
        let mut r = BufReader::with_capacity(1 << 20, File::open(&part)?);
        io::copy(&mut r, &mut HashWriter(&mut hasher))?;
    }

    let mut resp = with_retry(&file.url, || {
        // Ask for the stored bytes: they are what sources.toml pins.
        let req = agent.get(&file.url).header("Accept-Encoding", "identity");
        if have > 0 { req.header("Range", &format!("bytes={have}-")).call() } else { req.call() }
    })?;
    let status = resp.status().as_u16();
    let resuming = have > 0 && status == 206;
    if have > 0 && !resuming {
        info!(url = %file.url, "server ignored range request; restarting download");
        hasher = Sha256::new();
        have = 0;
    }
    let total = resp
        .headers()
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
        .map(|len| len + have);
    if let Some(t) = total
        && t > LARGE_DOWNLOAD_BYTES
        && !allow_large
    {
        bail!("{} is {:.1} GB; pass --allow-large to download it", file.url, t as f64 / 1e9);
    }
    info!(url = %file.url, total = ?total, resume_from = have, "downloading");

    let mut out = OpenOptions::new()
        .create(true)
        .write(true)
        .append(resuming)
        .truncate(!resuming)
        .open(&part)?;
    let mut reader = resp.body_mut().as_reader();
    let mut buf = vec![0u8; 1 << 20];
    let mut size = have;
    let mut next_log = size + (256 << 20);
    loop {
        let n = reader.read(&mut buf).with_context(|| format!("reading {}", file.url))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        out.write_all(&buf[..n])?;
        size += n as u64;
        if size >= next_log {
            info!(url = %file.url, mb = size >> 20, total_mb = ?total.map(|t| t >> 20), "progress");
            next_log += 256 << 20;
        }
    }
    out.sync_all()?;
    drop(out);
    if let Some(t) = total
        && t != size
    {
        bail!("{}: got {size} bytes, expected {t}; rerun to resume", file.url);
    }
    fs::rename(&part, dest)?;
    Ok((hex::encode(hasher.finalize()), size))
}

/// Extracts one member of a remote zip to `dest` via `<dest>.part`. Returns (sha256, size).
fn extract_member(
    archive: &mut RemoteArchive,
    url: &str,
    member: &str,
    dest: &Path,
) -> Result<(String, u64)> {
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut part = dest.as_os_str().to_owned();
    part.push(".part");
    let part = PathBuf::from(part);
    let mut entry =
        archive.by_name(member).with_context(|| format!("member `{member}` not found in {url}"))?;
    info!(%url, member, mb = entry.compressed_size() >> 20, "extracting zip member");
    let mut hasher = Sha256::new();
    let mut out = File::create(&part)?;
    let mut buf = vec![0u8; 1 << 20];
    let mut size = 0u64;
    loop {
        // The zip reader verifies the member's CRC-32 at the end.
        let n = entry.read(&mut buf).with_context(|| format!("reading `{member}` from {url}"))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        out.write_all(&buf[..n])?;
        size += n as u64;
    }
    out.sync_all()?;
    drop(out);
    fs::rename(&part, dest)?;
    Ok((hex::encode(hasher.finalize()), size))
}

/// Extracts every member under `prefix` into the directory `dest` (via `<dest>.part`).
/// Member names are sanitized by the zip crate, so archives cannot write outside `dest`.
fn extract_prefix(
    archive: &mut RemoteArchive,
    url: &str,
    prefix: &str,
    dest: &Path,
) -> Result<(String, u64)> {
    let mut part = dest.as_os_str().to_owned();
    part.push(".part");
    let part = PathBuf::from(part);
    let _ = fs::remove_dir_all(&part);
    fs::create_dir_all(&part)?;
    let mut names = Vec::new();
    for n in archive.file_names() {
        let n = n.with_context(|| format!("member name in {url}"))?;
        if n.starts_with(prefix) && !n.ends_with('/') {
            names.push(n.into_owned());
        }
    }
    if names.is_empty() {
        bail!("no members under `{prefix}` in {url}");
    }
    info!(%url, prefix, members = names.len(), "extracting zip folder");
    for name in &names {
        let mut entry = archive.by_name(name)?;
        let rel = entry
            .enclosed_name()
            .with_context(|| format!("unsafe member name `{name}` in {url}"))?;
        let rel = rel.strip_prefix(prefix.trim_end_matches('/')).unwrap_or(&rel).to_path_buf();
        let out_path = part.join(rel);
        if let Some(parent) = out_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut out = File::create(&out_path)?;
        io::copy(&mut entry, &mut out).with_context(|| format!("reading `{name}` from {url}"))?;
    }
    let _ = fs::remove_dir_all(dest);
    fs::rename(&part, dest)?;
    hash_dir(dest)
}

/// Writes newly computed hashes back into `sources.toml`, preserving its formatting.
fn pin(sources_toml: &Path, sources: &Sources, pins: &[(usize, usize, String, u64)]) -> Result<()> {
    let text = fs::read_to_string(sources_toml)?;
    let mut doc: toml_edit::DocumentMut = text.parse()?;
    let today = today();
    // Extra file lists (data/sources/<id>.toml), loaded on demand.
    let mut extras: HashMap<usize, toml_edit::DocumentMut> = HashMap::new();
    {
        let srcs = doc["source"]
            .as_array_of_tables_mut()
            .context("sources.toml: `source` is not an array of tables")?;
        for (si, fi, sha, size) in pins {
            let main = sources.sources[*si].main_files;
            let src = srcs.get_mut(*si).context("source index")?;
            src["retrieved"] = toml_edit::value(today.clone());
            let f = if *fi < main {
                let files = src["files"].as_array_of_tables_mut().context("files")?;
                files.get_mut(*fi).context("file index")?
            } else {
                let e = match extras.entry(*si) {
                    Entry::Occupied(e) => e.into_mut(),
                    Entry::Vacant(e) => {
                        let p = Sources::extra_path(sources_toml, &sources.sources[*si].id);
                        e.insert(fs::read_to_string(&p)?.parse()?)
                    }
                };
                let files = e["files"].as_array_of_tables_mut().context("files")?;
                files.get_mut(*fi - main).context("extra file index")?
            };
            f["sha256"] = toml_edit::value(sha.clone());
            f["size_bytes"] = toml_edit::value(*size as i64);
        }
    }
    for (si, e) in &extras {
        fs::write(Sources::extra_path(sources_toml, &sources.sources[*si].id), e.to_string())?;
    }
    // Refresh per-source totals over all files, extra lists included.
    let srcs = doc["source"].as_array_of_tables_mut().context("source")?;
    for (si, src) in srcs.iter_mut().enumerate() {
        let mut sizes: Vec<Option<i64>> = src
            .get("files")
            .and_then(|f| f.as_array_of_tables())
            .map(|fs| fs.iter().map(|f| f.get("size_bytes").and_then(|v| v.as_integer())).collect())
            .unwrap_or_default();
        let extra = Sources::extra_path(sources_toml, &sources.sources[si].id);
        let extra_doc = match extras.get(&si) {
            Some(d) => Some(d.clone()),
            None if extra.exists() => {
                Some(fs::read_to_string(&extra)?.parse::<toml_edit::DocumentMut>()?)
            }
            None => None,
        };
        if let Some(d) = extra_doc
            && let Some(fs) = d.get("files").and_then(|f| f.as_array_of_tables())
        {
            sizes.extend(fs.iter().map(|f| f.get("size_bytes").and_then(|v| v.as_integer())));
        }
        let known: Vec<i64> = sizes.iter().flatten().copied().collect();
        if !known.is_empty() && known.len() == sizes.len() {
            src["size_bytes"] = toml_edit::value(known.iter().sum::<i64>());
        }
    }
    fs::write(sources_toml, doc.to_string())?;
    info!(count = pins.len(), "pinned new checksums");
    Ok(())
}

/// Current UTC date as `YYYY-MM-DD` (civil-from-days, H. Hinnant).
fn today() -> String {
    let days = (SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() / 86_400) as i64;
    civil_date(days)
}

fn civil_date(days_since_epoch: i64) -> String {
    let z = days_since_epoch + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_dates() {
        assert_eq!(civil_date(0), "1970-01-01");
        assert_eq!(civil_date(19_782), "2024-02-29");
        assert_eq!(civil_date(20_752), "2026-10-26");
    }
}
