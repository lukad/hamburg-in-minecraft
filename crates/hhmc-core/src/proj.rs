//! Coordinate transformation by shelling out to PROJ's `cs2cs`, which keeps
//! libproj out of the Rust build (the same approach as GDAL/OGR via their CLIs).

use std::io::Write;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail, ensure};

use crate::geom::Bbox;

/// Transforms WGS84 `(lat, lon)` points to EPSG:25832 `(E, N)` with `cs2cs`.
pub fn wgs84_to_25832(points: &[(f64, f64)]) -> Result<Vec<(f64, f64)>> {
    cs2cs(points, "EPSG:4326", "EPSG:25832", "%.4f")
}

/// Transforms EPSG:25832 `(E, N)` points to WGS84 `(lat, lon)` with `cs2cs`.
pub fn epsg25832_to_wgs84(points: &[(f64, f64)]) -> Result<Vec<(f64, f64)>> {
    cs2cs(points, "EPSG:25832", "EPSG:4326", "%.9f")
}

/// Runs `cs2cs` on 2-D points (axis order as the CRSs define it).
fn cs2cs(points: &[(f64, f64)], from: &str, to: &str, format: &str) -> Result<Vec<(f64, f64)>> {
    let mut child = Command::new("cs2cs")
        .args(["-f", format, from, to])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("running cs2cs (install PROJ)")?;
    // Written from a thread: cs2cs answers line by line, and with large batches its output
    // pipe fills while the input is still being written.
    let mut stdin = child.stdin.take().expect("piped stdin");
    let input: String = points.iter().map(|(a, b)| format!("{a} {b}\n")).collect();
    let writer = std::thread::spawn(move || -> std::io::Result<()> {
        // EPSG:4326 axis order is lat, lon; EPSG:25832 is E, N.
        stdin.write_all(input.as_bytes())
    });
    let out = child.wait_with_output()?;
    writer.join().map_err(|_| anyhow::anyhow!("cs2cs writer panicked"))??;
    if !out.status.success() {
        bail!("cs2cs failed: {}", String::from_utf8_lossy(&out.stderr));
    }
    let text = String::from_utf8(out.stdout)?;
    let res: Vec<(f64, f64)> = text
        .lines()
        .map(|l| {
            let mut it = l.split_whitespace().map(str::parse::<f64>);
            match (it.next(), it.next()) {
                (Some(Ok(e)), Some(Ok(n))) => Ok((e, n)),
                _ => bail!("unexpected cs2cs output line `{l}`"),
            }
        })
        .collect::<Result<_>>()?;
    ensure!(res.len() == points.len(), "cs2cs returned {} of {} points", res.len(), points.len());
    Ok(res)
}

/// EPSG:25832 envelope of the WGS84 box `sw`–`ne` (`[lat, lon]`). Each edge is
/// densified so the curvature of parallels and meridians in UTM is captured.
pub fn wgs84_box_to_25832(sw: [f64; 2], ne: [f64; 2]) -> Result<Bbox> {
    const STEPS: usize = 32;
    let mut pts = Vec::with_capacity(4 * (STEPS + 1));
    for i in 0..=STEPS {
        let t = i as f64 / STEPS as f64;
        let lat = sw[0] + t * (ne[0] - sw[0]);
        let lon = sw[1] + t * (ne[1] - sw[1]);
        pts.extend([(sw[0], lon), (ne[0], lon), (lat, sw[1]), (lat, ne[1])]);
    }
    let utm = wgs84_to_25832(&pts)?;
    Ok(Bbox::from_points(utm).expect("non-empty"))
}
