//! `cargo run --release -p hhmc-citygml --example stats -- <files…>`: building, part,
//! polygon and triangle counts per CityGML file.

use std::io::BufReader;

fn main() -> anyhow::Result<()> {
    for path in std::env::args().skip(1) {
        let t = std::time::Instant::now();
        let (mut b, mut parts, mut polys, mut tris, mut empty) = (0, 0, 0, 0, 0);
        hhmc_citygml::parse_buildings(BufReader::new(std::fs::File::open(&path)?), |bl| {
            b += 1;
            parts += bl.parts.len();
            let vol: Vec<_> = bl.parts.iter().flat_map(|p| p.volume_polygons()).collect();
            if vol.is_empty() {
                empty += 1;
            }
            polys += vol.len();
            tris += vol.iter().map(|p| hhmc_citygml::triangulate(p).len()).sum::<usize>();
        })?;
        println!(
            "{path}: {b} buildings, {parts} parts, {polys} polygons, {tris} triangles, {empty} without geometry, {:.2} s",
            t.elapsed().as_secs_f64()
        );
    }
    Ok(())
}
