//! Prints the y of the highest non-air block (ignoring leaves/logs) for a window of world
//! columns: `cargo run --example topgrid -- <region dir> <x0> <x1> <z0> <z1>`.
use hhmc_core::TileKey;
use std::collections::HashMap;

fn main() -> anyhow::Result<()> {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let n = |i: usize| a[i].parse::<i32>().unwrap();
    let (x0, x1, z0, z1) = (n(1), n(2), n(3), n(4));
    let mut tiles: HashMap<TileKey, hhmc_core::VoxelTile> = HashMap::new();
    print!("      ");
    for x in x0..=x1 {
        print!("{:>4}", x.rem_euclid(1000));
    }
    println!();
    for z in z0..=z1 {
        print!("{z:5} ");
        for x in x0..=x1 {
            let t = TileKey::from_block(x, z);
            if let std::collections::hash_map::Entry::Vacant(v) = tiles.entry(t) {
                let path = std::path::Path::new(&a[0]).join(t.region_file_name());
                v.insert(hhmc_world::read_tile(&path, t)?);
            }
            let tile = &tiles[&t];
            let (bx, bz) = t.min_block();
            let mut s = String::from("   .");
            for y in (-64..320).rev() {
                let b = tile.palette.get(tile.get((x - bx) as usize, y, (z - bz) as usize));
                if b.is_air() || b.name.ends_with("_leaves") || b.name.ends_with("_log") {
                    continue;
                }
                let c = match &*b.name {
                    "minecraft:water" => 'w',
                    "minecraft:iron_bars" => 'r',
                    "minecraft:rail" => 't',
                    n if n.contains("andesite")
                        || n.contains("concrete")
                        || n.contains("smooth_stone") =>
                    {
                        ' '
                    }
                    _ => '#',
                };
                s = format!("{y:>3}{c}");
                break;
            }
            print!("{s}");
        }
        println!();
    }
    Ok(())
}
