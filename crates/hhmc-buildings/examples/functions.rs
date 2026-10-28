//! Counts building functions (CityGML `bldg:function`) in prepared bins, with examples:
//! `cargo run --example functions -- <bin>...`
fn main() -> anyhow::Result<()> {
    let mut seen = std::collections::HashSet::new();
    let mut count: std::collections::BTreeMap<String, (usize, String)> = Default::default();
    for p in std::env::args().skip(1) {
        for m in hhmc_buildings::load_meshes(std::path::Path::new(&p))? {
            if seen.insert(m.id.clone()) {
                let e = count
                    .entry(m.function.clone().unwrap_or_default())
                    .or_insert((0, m.id.clone()));
                e.0 += 1;
            }
        }
    }
    for (f, (n, ex)) in count {
        println!("{n:6} {f} (e.g. {ex})");
    }
    Ok(())
}
