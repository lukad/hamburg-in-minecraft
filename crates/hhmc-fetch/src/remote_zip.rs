//! Reading single members of large remote zip archives with HTTP range requests.
//!
//! The LGV publishes its rasters only as city- or district-wide zips (up to several GB),
//! but its server honors `Range`, so the central directory and the wanted members are
//! all that is transferred.

use std::io::{self, Read, Seek, SeekFrom};
use std::thread::sleep;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tracing::warn;

/// Bytes fetched by a range request that continues the previous one (streaming a member).
const BLOCK: u64 = 8 << 20;
/// Bytes fetched by a range request elsewhere (directory records, local headers).
const SMALL_BLOCK: u64 = 256 << 10;
const MAX_ATTEMPTS: u32 = 7;

/// `Read + Seek` over a remote file, backed by HTTP range requests.
pub struct HttpRangeReader {
    agent: ureq::Agent,
    url: String,
    len: u64,
    pos: u64,
    block_start: u64,
    block: Vec<u8>,
}

impl HttpRangeReader {
    pub fn open(agent: &ureq::Agent, url: &str) -> Result<Self> {
        // The length comes from a ranged GET, not HEAD: when a file has just been
        // republished, the CDN can answer HEAD from a stale copy while range requests
        // reach the new one.
        let resp = with_retry(url, || {
            agent.get(url).header("Range", "bytes=0-0").header("Accept-Encoding", "identity").call()
        })?;
        if resp.status().as_u16() != 206 {
            bail!("GET {url}: server ignored the range request ({})", resp.status());
        }
        let len = total_len(&resp).with_context(|| format!("GET {url}: no Content-Range total"))?;
        Ok(Self {
            agent: agent.clone(),
            url: url.to_owned(),
            len,
            pos: 0,
            block_start: 0,
            block: Vec::new(),
        })
    }

    fn fetch(&mut self, start: u64, want: u64) -> io::Result<()> {
        let sequential =
            !self.block.is_empty() && start == self.block_start + self.block.len() as u64;
        let block = if sequential { BLOCK } else { SMALL_BLOCK };
        let end = (start + want.max(block)).min(self.len) - 1;
        let range = format!("bytes={start}-{end}");
        // A connection dropped while reading the body is retried like a failed request.
        let mut delay = Duration::from_secs(2);
        let mut attempt = 1;
        let buf = loop {
            let mut resp = with_retry(&self.url, || {
                self.agent
                    .get(&self.url)
                    .header("Range", &range)
                    .header("Accept-Encoding", "identity")
                    .call()
            })
            .map_err(io::Error::other)?;
            if resp.status().as_u16() != 206 {
                return Err(io::Error::other(format!(
                    "{}: expected 206 for {range}, got {}",
                    self.url,
                    resp.status()
                )));
            }
            let mut buf = Vec::with_capacity((end - start + 1) as usize);
            // Another length: a CDN edge still serving a replaced file's old version (the
            // zip CRCs and pinned sha256 catch anything that slips through).
            let error = if total_len(&resp) != Some(self.len) {
                format!(
                    "the file changed while reading it (length {:?}, expected {})",
                    total_len(&resp),
                    self.len
                )
            } else {
                match resp.body_mut().as_reader().read_to_end(&mut buf) {
                    Ok(_) if buf.len() as u64 == end - start + 1 => break buf,
                    Ok(_) => format!("short range response for {range}"),
                    Err(e) => e.to_string(),
                }
            };
            if attempt == MAX_ATTEMPTS {
                return Err(io::Error::other(format!("{}: {error}", self.url)));
            }
            warn!(url = %self.url, attempt, %error, wait_s = delay.as_secs(), "range read failed, retrying");
            sleep(delay);
            delay *= 2;
            attempt += 1;
        };
        self.block_start = start;
        self.block = buf;
        Ok(())
    }
}

/// The total length from a 206 response's `Content-Range: bytes a-b/<total>`.
fn total_len(resp: &ureq::http::Response<ureq::Body>) -> Option<u64> {
    let v = resp.headers().get("content-range")?.to_str().ok()?;
    v.rsplit_once('/')?.1.trim().parse().ok()
}

impl Read for HttpRangeReader {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if self.pos >= self.len || out.is_empty() {
            return Ok(0);
        }
        let in_block =
            self.pos >= self.block_start && self.pos < self.block_start + self.block.len() as u64;
        if !in_block {
            self.fetch(self.pos, out.len() as u64)?;
        }
        let off = (self.pos - self.block_start) as usize;
        let n = out.len().min(self.block.len() - off);
        out[..n].copy_from_slice(&self.block[off..off + n]);
        self.pos += n as u64;
        Ok(n)
    }
}

impl Seek for HttpRangeReader {
    fn seek(&mut self, to: SeekFrom) -> io::Result<u64> {
        let new = match to {
            SeekFrom::Start(p) => p as i128,
            SeekFrom::End(d) => self.len as i128 + d as i128,
            SeekFrom::Current(d) => self.pos as i128 + d as i128,
        };
        if new < 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "seek before start"));
        }
        self.pos = new as u64;
        Ok(self.pos)
    }
}

/// Retries transient failures (429, 5xx, transport errors) with exponential backoff.
pub(crate) fn with_retry<T>(url: &str, mut f: impl FnMut() -> Result<T, ureq::Error>) -> Result<T> {
    let mut delay = Duration::from_secs(2);
    for attempt in 1..=MAX_ATTEMPTS {
        match f() {
            Ok(v) => return Ok(v),
            Err(e) => {
                let transient = match &e {
                    ureq::Error::StatusCode(c) => *c == 429 || *c >= 500,
                    ureq::Error::Io(_)
                    | ureq::Error::Timeout(_)
                    | ureq::Error::ConnectionFailed => true,
                    _ => false,
                };
                if !transient || attempt == MAX_ATTEMPTS {
                    return Err(e).with_context(|| format!("GET {url}"));
                }
                warn!(%url, attempt, error = %e, wait_s = delay.as_secs(), "transient error, retrying");
                sleep(delay);
                delay *= 2;
            }
        }
    }
    unreachable!()
}

/// One member of a remote zip, as listed by [`list_remote_zip`].
#[derive(Debug, Clone)]
pub struct RemoteMember {
    pub name: String,
    pub compressed_size: u64,
    pub size: u64,
    pub crc32: u32,
}

/// Lists a remote zip's members by reading only its central directory.
pub fn list_remote_zip(url: &str) -> Result<Vec<RemoteMember>> {
    let agent = crate::download::agent();
    let zip = zip::ZipArchive::new(HttpRangeReader::open(&agent, url)?)
        .with_context(|| format!("zip {url}"))?;
    (0..zip.len())
        .map(|i| {
            // Central-directory data only; opening entries would fetch every local header.
            let e = zip.by_index_data(i)?;
            Ok(RemoteMember {
                name: e.name()?.into_owned(),
                compressed_size: e.compressed_size(),
                size: e.size(),
                crc32: e.crc32(),
            })
        })
        .collect()
}
