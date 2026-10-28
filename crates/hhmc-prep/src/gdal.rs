//! Running GDAL/OGR command-line tools.

use std::ffi::OsStr;
use std::process::Command;

use anyhow::{Context, Result, bail};
use tracing::{debug, warn};

/// Runs a GDAL tool; fails with its stderr on a non-zero exit. Plugin-loading noise from
/// unrelated drivers (e.g. a missing libarrow on the host) is dropped from the message.
pub fn run<I, S>(tool: &str, args: I) -> Result<()>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let args: Vec<_> = args.into_iter().map(|a| a.as_ref().to_owned()).collect();
    debug!(tool, ?args, "running");
    // A tool killed by a signal is retried: gdalwarp can crash under full parallel load
    // and succeed on the same input alone.
    let mut attempt = 1;
    let out = loop {
        let out = Command::new(tool)
            .args(&args)
            .env("GDAL_PAM_ENABLED", "NO")
            .output()
            .with_context(|| format!("running {tool} (install GDAL)"))?;
        if out.status.code().is_some() || attempt == 3 {
            break out;
        }
        warn!(tool, status = %out.status, attempt, "killed by a signal; retrying");
        attempt += 1;
    };
    if !out.status.success() {
        let err: Vec<&str> = std::str::from_utf8(&out.stderr)
            .unwrap_or("")
            .lines()
            .filter(|l| !l.contains("libarrow"))
            .collect();
        bail!("{tool} failed ({}):\n{}", out.status, err.join("\n"));
    }
    Ok(())
}
