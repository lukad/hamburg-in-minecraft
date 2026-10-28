//! Prints the top block (first letter code) of a window of world columns, ignoring trees:
//! `cargo run --example blockgrid -- <region dir> <x0> <x1> <z0> <z1>`.
//! r road (gray_concrete), s sidewalk (smooth_stone), p plaza (polished_andesite),
//! g gravel, w water, b bars, t rail, # other, . air.
use hhmc_core::TileKey;
use std::collections::HashMap;

fn main() -> anyhow::Result<()> {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let n = |i: usize| a[i].parse::<i32>().unwrap();
    let (x0, x1, z0, z1) = (n(1), n(2), n(3), n(4));
    let mut tiles: HashMap<TileKey, hhmc_core::VoxelTile> = HashMap::new();
    for z in z0..=z1 {
        let mut row = String::new();
        for x in x0..=x1 {
            let t = TileKey::from_block(x, z);
            if let std::collections::hash_map::Entry::Vacant(v) = tiles.entry(t) {
                let path = std::path::Path::new(&a[0]).join(t.region_file_name());
                v.insert(hhmc_world::read_tile(&path, t)?);
            }
            let tile = &tiles[&t];
            let (bx, bz) = t.min_block();
            let mut c = '.';
            for y in (-64..320).rev() {
                let b = tile.palette.get(tile.get((x - bx) as usize, y, (z - bz) as usize));
                if b.is_air() || b.name.ends_with("_leaves") || b.name.ends_with("_log") {
                    continue;
                }
                c = match &*b.name {
                    "minecraft:gray_concrete" => 'r',
                    "minecraft:smooth_stone" => 's',
                    "minecraft:polished_andesite" => 'p',
                    "minecraft:gravel" => 'g',
                    "minecraft:water" => 'w',
                    "minecraft:iron_bars" => 'b',
                    "minecraft:rail" => 't',
                    _ => '#',
                };
                break;
            }
            row.push(c);
        }
        println!("{z:5} {row}");
    }
    Ok(())
}
