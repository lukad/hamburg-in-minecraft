//! Lists the meshes of a prepared tile bin taller than a height: id, source, function,
//! bounding box. `cargo run --example tall -- <bin> <min height m>`.
fn main() -> anyhow::Result<()> {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let min: f64 = a[1].parse()?;
    for m in hhmc_buildings::load_meshes(std::path::Path::new(&a[0]))? {
        if m.bbox[5] - m.bbox[2] >= min {
            println!(
                "{} source={} function={:?} parts={} E {:.0}..{:.0} N {:.0}..{:.0} H {:.1}..{:.1}",
                m.id,
                m.source,
                m.function,
                m.parts.len(),
                m.bbox[0],
                m.bbox[3],
                m.bbox[1],
                m.bbox[4],
                m.bbox[2],
                m.bbox[5]
            );
        }
    }
    Ok(())
}
