//! Prints the blocks of world columns from y0 to y1:
//! `cargo run --example column -- <region dir> <y0> <y1> <x,z>...` (world blocks).
use hhmc_core::TileKey;

fn main() -> anyhow::Result<()> {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let (y0, y1): (i32, i32) = (a[1].parse()?, a[2].parse()?);
    for c in &a[3..] {
        let (x, z) = c.split_once(',').expect("x,z");
        let (x, z): (i32, i32) = (x.parse()?, z.parse()?);
        let t = TileKey::from_block(x, z);
        let tile =
            hhmc_world::read_tile(&std::path::Path::new(&a[0]).join(t.region_file_name()), t)?;
        let (bx, bz) = t.min_block();
        println!("({x}, {z}):");
        for y in (y0..=y1).rev() {
            let s = tile.palette.get(tile.get((x - bx) as usize, y, (z - bz) as usize));
            if !s.is_air() {
                println!("  {y:4} {s}");
            }
        }
    }
    Ok(())
}
