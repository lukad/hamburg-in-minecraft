//! Server-based checks: load every chunk of a world's tiles and assert blocks via
//! `/execute if block`, then scan the log for chunk errors.

use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use hhmc_core::{BlockState, TileKey};
use tracing::info;

use crate::server::{ServerSetup, problem_lines};

#[derive(Clone, Debug)]
pub struct BlockAssert {
    pub x: i32,
    pub y: i32,
    pub z: i32,
    pub state: BlockState,
}

#[derive(Debug, Default, serde::Serialize)]
pub struct HarnessReport {
    pub chunks_loaded: usize,
    pub asserts: usize,
    pub failures: Vec<String>,
    pub log_problems: Vec<String>,
}

impl HarnessReport {
    pub fn passed(&self) -> bool {
        self.failures.is_empty() && self.log_problems.is_empty()
    }
}

/// Runs the server on `<universe>/<world>`, force-loads every chunk of `tiles` in batches
/// of 16 × 16 chunks (the `/forceload` limit is 256 chunks), checks `asserts` once their
/// chunks are loaded, stops the server and scans its log.
pub fn run(
    setup: &ServerSetup,
    universe: &Path,
    world: &str,
    tiles: &[TileKey],
    asserts: &[BlockAssert],
) -> Result<HarnessReport> {
    let mut srv = setup.start(universe, world, &[])?;
    let mut report = HarnessReport::default();
    for t in tiles {
        let (bx, bz) = t.min_block();
        for qz in 0..2 {
            for qx in 0..2 {
                let (x0, z0) = (bx + qx * 256, bz + qz * 256);
                let r =
                    srv.command(&format!("forceload add {x0} {z0} {} {}", x0 + 255, z0 + 255))?;
                if !r.contains("Marked") && !r.contains("marked") && !r.contains("already") {
                    bail!("forceload failed: {r}");
                }
                // Every chunk of the batch must load (this is the "game load" check).
                wait_all_loaded(&mut srv, x0, z0)?;
                report.chunks_loaded += 256;
                // Check this batch's assertions while it is loaded.
                for a in asserts
                    .iter()
                    .filter(|a| (x0..x0 + 256).contains(&a.x) && (z0..z0 + 256).contains(&a.z))
                {
                    report.asserts += 1;
                    let cmd = format!("execute if block {} {} {} {}", a.x, a.y, a.z, a.state);
                    let r = srv.command(&cmd)?;
                    if !r.contains("Test passed") {
                        report.failures.push(format!("{cmd} → {r}"));
                    }
                }
                srv.command(&format!("forceload remove {x0} {z0} {} {}", x0 + 255, z0 + 255))?;
            }
        }
        info!(tile = %t, "tile loaded and checked");
    }
    // The world is a throwaway copy, so skip the server's save on shutdown.
    let log = srv.abort()?;
    report.log_problems = problem_lines(&log);
    Ok(report)
}

/// Waits until all 16 × 16 chunks whose north-west block is `(x0, z0)` are loaded.
fn wait_all_loaded(srv: &mut crate::server::Running, x0: i32, z0: i32) -> Result<()> {
    let mut pending: Vec<(i32, i32)> = (0..16)
        .flat_map(|cz| (0..16).map(move |cx| (x0 + cx * 16 + 8, z0 + cz * 16 + 8)))
        .collect();
    let deadline = Instant::now() + Duration::from_secs(300);
    while !pending.is_empty() {
        let mut still = Vec::new();
        for (x, z) in pending {
            let r = srv.command(&format!("execute if loaded {x} 0 {z}"))?;
            if !r.contains("Test passed") {
                still.push((x, z));
            }
        }
        pending = still;
        if pending.is_empty() {
            break;
        }
        if Instant::now() > deadline {
            bail!("{} chunks near {x0},{z0} did not load", pending.len());
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    Ok(())
}
