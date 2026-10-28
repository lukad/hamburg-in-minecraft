//! `cargo run --release -p hhmc-citygml --example tricheck -- <files…>`: polygons whose
//! triangulated area differs from their (Newell) area by more than 1 %.

use std::io::BufReader;

fn norm(v: [f64; 3]) -> f64 {
    (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt()
}

fn main() -> anyhow::Result<()> {
    let (mut polys, mut bad, mut bad_area) = (0usize, 0usize, 0.0f64);
    let mut examples = Vec::new();
    for path in std::env::args().skip(1) {
        hhmc_citygml::parse_buildings(BufReader::new(std::fs::File::open(&path)?), |b| {
            for p in b.parts.iter().flat_map(|p| p.volume_polygons()) {
                polys += 1;
                let mut area = norm(hhmc_citygml::newell(&p.rings[0])) / 2.0;
                for h in &p.rings[1..] {
                    area -= norm(hhmc_citygml::newell(h)) / 2.0;
                }
                let tri: f64 = hhmc_citygml::triangulate(p)
                    .iter()
                    .map(|t| {
                        let u = [t[1][0] - t[0][0], t[1][1] - t[0][1], t[1][2] - t[0][2]];
                        let v = [t[2][0] - t[0][0], t[2][1] - t[0][1], t[2][2] - t[0][2]];
                        norm([
                            u[1] * v[2] - u[2] * v[1],
                            u[2] * v[0] - u[0] * v[2],
                            u[0] * v[1] - u[1] * v[0],
                        ]) / 2.0
                    })
                    .sum();
                if area > 0.01 && (tri - area).abs() > 0.01 * area {
                    bad += 1;
                    bad_area += (tri - area).abs();
                    if examples.len() < 8 {
                        examples.push(format!(
                            "{} {:?} rings={} pts={} area={area:.2} tri={tri:.2}",
                            b.id,
                            p.kind,
                            p.rings.len(),
                            p.rings[0].len()
                        ));
                    }
                }
            }
        })?;
    }
    println!(
        "{polys} polygons, {bad} with triangulated area off by > 1 % (total |Δ| {bad_area:.1} m²)"
    );
    for e in examples {
        println!("  {e}");
    }
    Ok(())
}
