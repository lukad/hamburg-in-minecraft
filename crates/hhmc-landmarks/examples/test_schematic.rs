//! Writes a Sponge Schematic v3 test landmark: a 7 × 4 × 9 gold platform with a quartz
//! arrow pointing north (−z), north-facing stairs at its tip and a glass post at its
//! north-east corner, so position, rotation and block-state turns are visible.
//!
//! `cargo run -p hhmc-landmarks --example test_schematic -- <out.schem>`

use std::collections::HashMap;
use std::io::Write;

use fastnbt::Value;

fn main() -> anyhow::Result<()> {
    let out = std::env::args()
        .nth(1)
        .ok_or_else(|| anyhow::anyhow!("usage: test_schematic <out.schem>"))?;
    let (w, h, l) = (7i32, 4i32, 9i32);
    let mut names: Vec<String> = vec!["minecraft:air".into()];
    let idx = |s: &str, names: &mut Vec<String>| -> u32 {
        match names.iter().position(|n| n == s) {
            Some(i) => i as u32,
            None => {
                names.push(s.to_string());
                names.len() as u32 - 1
            }
        }
    };
    let mut grid = vec![0u32; (w * h * l) as usize];
    let mut set = |x: i32, y: i32, z: i32, s: &str, names: &mut Vec<String>| {
        grid[(x + z * w + y * w * l) as usize] = idx(s, names);
    };
    for z in 0..l {
        for x in 0..w {
            set(x, 0, z, "minecraft:gold_block", &mut names);
        }
    }
    // Arrow shaft along z, head at the north (z = 1), stairs facing north at the tip.
    for z in 2..l - 1 {
        set(3, 1, z, "minecraft:quartz_block", &mut names);
    }
    for (x, z) in [(2, 2), (4, 2), (1, 3), (5, 3)] {
        set(x, 1, z, "minecraft:quartz_block", &mut names);
    }
    set(
        3,
        1,
        1,
        "minecraft:quartz_stairs[facing=north,half=bottom,shape=straight,waterlogged=false]",
        &mut names,
    );
    for y in 1..h {
        set(w - 1, y, 0, "minecraft:glass", &mut names);
    }
    let mut data = Vec::new();
    for mut v in grid {
        loop {
            let b = (v & 0x7f) as u8;
            v >>= 7;
            if v == 0 {
                data.push(b as i8);
                break;
            }
            data.push((b | 0x80) as i8);
        }
    }
    let c = |v: Vec<(&str, Value)>| {
        Value::Compound(v.into_iter().map(|(k, v)| (k.to_string(), v)).collect::<HashMap<_, _>>())
    };
    let palette = Value::Compound(
        names.iter().enumerate().map(|(i, n)| (n.clone(), Value::Int(i as i32))).collect(),
    );
    let schem = c(vec![
        ("Version", Value::Int(3)),
        ("DataVersion", Value::Int(5023)),
        ("Width", Value::Short(w as i16)),
        ("Height", Value::Short(h as i16)),
        ("Length", Value::Short(l as i16)),
        ("Offset", Value::IntArray(fastnbt::IntArray::new(vec![0, 0, 0]))),
        (
            "Blocks",
            c(vec![
                ("Palette", palette),
                ("Data", Value::ByteArray(fastnbt::ByteArray::new(data))),
                ("BlockEntities", Value::List(vec![])),
            ]),
        ),
    ]);
    let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    e.write_all(&fastnbt::to_bytes(&c(vec![("Schematic", schem)]))?)?;
    std::fs::write(&out, e.finish()?)?;
    println!("wrote {out}");
    Ok(())
}
