//! Prints horizontal slices of a written region as characters, for inspecting voxel
//! geometry: `cargo run --example slice -- <region.mca> <rx> <rz> <x0> <x1> <z0> <z1> <y>...`
//! (tile-local x/z). Legend: `.` air, `#` solid, `g` glass, `~` water, `*` leaves/log.
use hhmc_core::TileKey;

fn main() -> anyhow::Result<()> {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let n = |i: usize| a[i].parse::<i32>().unwrap();
    let t = hhmc_world::read_tile(std::path::Path::new(&a[0]), TileKey::new(n(1), n(2)))?;
    let (x0, x1, z0, z1) = (n(3), n(4), n(5), n(6));
    for y in a[7..].iter().map(|s| s.parse::<i32>().unwrap()) {
        println!("y = {y}");
        for z in z0..=z1 {
            let row: String = (x0..=x1)
                .map(|x| {
                    let s = t.palette.get(t.get(x as usize, y, z as usize));
                    match &*s.name {
                        "minecraft:air" => '.',
                        "minecraft:water" => '~',
                        n if n.contains("glass") => 'g',
                        n if n.ends_with("_leaves") || n.ends_with("_log") => '*',
                        _ => '#',
                    }
                })
                .collect();
            println!("{z:4} {row}");
        }
    }
    Ok(())
}
