//! Lists buildings whose open geometry was filled (enclosed cavities), largest first:
//! `cargo run --example fills -- <bin>...`
use hhmc_core::config::Origin;

fn main() -> anyhow::Result<()> {
    let origin = Origin { e0: 565_760, n0: 5_934_080 };
    let mut seen = std::collections::HashSet::new();
    let mut rows = Vec::new();
    for p in std::env::args().skip(1) {
        for m in hhmc_buildings::load_meshes(std::path::Path::new(&p))? {
            if !seen.insert(m.id.clone()) {
                continue;
            }
            let v = hhmc_buildings::voxelize(&m, &origin);
            if v.installation_fill > 0 {
                let vol: usize = v
                    .columns
                    .values()
                    .flatten()
                    .map(|i| (i.top - i.bot.max(i.top - 400) + 1) as usize)
                    .sum();
                rows.push((
                    v.installation_fill,
                    vol,
                    m.id.clone(),
                    m.source.clone(),
                    [m.bbox[0], m.bbox[1]],
                ));
            }
        }
    }
    rows.sort_by_key(|r| std::cmp::Reverse(r.0));
    println!("{} buildings filled, {} voxels", rows.len(), rows.iter().map(|r| r.0).sum::<usize>());
    for r in rows.iter().take(25) {
        println!(
            "{:>7} fill / {:>8} volume  {} {} at {:.0} {:.0}",
            r.0, r.1, r.2, r.3, r.4[0], r.4[1]
        );
    }
    Ok(())
}
