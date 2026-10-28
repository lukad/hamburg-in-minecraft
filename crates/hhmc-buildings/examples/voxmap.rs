//! Prints where one building's voxels come from at given heights: `c` volume column,
//! `s` surface (installations and filled cavities).
//! `cargo run --example voxmap -- <bin> <id> <x0> <x1> <z0> <z1> <y>...` (world blocks).
use hhmc_core::config::Origin;

fn main() -> anyhow::Result<()> {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let n = |i: usize| a[i].parse::<i32>().unwrap();
    let meshes = hhmc_buildings::load_meshes(std::path::Path::new(&a[0]))?;
    let m = meshes.iter().find(|m| m.id == a[1]).expect("building in bin");
    let origin = Origin { e0: 565_760, n0: 5_934_080 };
    let v = hhmc_buildings::voxelize(m, &origin);
    let surf: std::collections::HashSet<_> = v.surface.iter().copied().collect();
    for y in a[6..].iter().map(|s| s.parse::<i32>().unwrap()) {
        println!("y = {y}");
        for z in n(4)..=n(5) {
            let row: String = (n(2)..=n(3))
                .map(|x| {
                    if v.contains(x, y, z) {
                        'c'
                    } else if surf.contains(&(x, y, z)) {
                        's'
                    } else {
                        '.'
                    }
                })
                .collect();
            println!("{z:4} {row}");
        }
    }
    Ok(())
}
