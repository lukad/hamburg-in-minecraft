//! A test schematic written in both formats, read back, and placed rotated across the
//! boundary between tiles (0, 0) and (1, 0) over generated blocks.

use std::collections::HashMap;
use std::io::Write;

use fastnbt::Value;
use hhmc_core::ground::GroundStats;
use hhmc_core::landcfg::BlocksConfig;
use hhmc_core::{TILE_BUFFER, VoxelTile};

use super::*;

const O: Origin = Origin { e0: 0, n0: 1000 };
const H: HeightMapping = HeightMapping { min_y: -64, max_y: 319, clamp_min: -63, clamp_max: 318 };

/// 3 × 2 × 2: stone, north-facing stairs and an x log on the bottom row (z 0), a fence
/// connected east at (0, 1, 1); the rest air.
fn test_blocks() -> Vec<([i32; 3], &'static str)> {
    vec![
        ([0, 0, 0], "minecraft:stone"),
        (
            [1, 0, 0],
            "minecraft:oak_stairs[facing=north,half=bottom,shape=straight,waterlogged=false]",
        ),
        ([2, 0, 0], "minecraft:oak_log[axis=x]"),
        (
            [0, 1, 1],
            "minecraft:oak_fence[east=true,north=false,south=false,waterlogged=false,west=false]",
        ),
    ]
}

fn gzip(b: &[u8]) -> Vec<u8> {
    let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    e.write_all(b).unwrap();
    e.finish().unwrap()
}

fn compound(v: Vec<(&str, Value)>) -> Value {
    Value::Compound(v.into_iter().map(|(k, v)| (k.to_string(), v)).collect::<HashMap<_, _>>())
}

/// Sponge Schematic v3 bytes of the test blocks.
fn sponge_bytes() -> Vec<u8> {
    let (w, h, l) = (3i32, 2i32, 2i32);
    let mut names = vec!["minecraft:air".to_string()];
    let mut grid = vec![0u32; (w * h * l) as usize];
    for (p, s) in test_blocks() {
        names.push(s.to_string());
        grid[(p[0] + p[2] * w + p[1] * w * l) as usize] = names.len() as u32 - 1;
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
    let palette = compound(
        names.iter().enumerate().map(|(i, n)| (n.as_str(), Value::Int(i as i32))).collect(),
    );
    let schem = compound(vec![
        ("Version", Value::Int(3)),
        ("DataVersion", Value::Int(5023)),
        ("Width", Value::Short(w as i16)),
        ("Height", Value::Short(h as i16)),
        ("Length", Value::Short(l as i16)),
        ("Offset", Value::IntArray(fastnbt::IntArray::new(vec![0, 0, 0]))),
        (
            "Blocks",
            compound(vec![
                ("Palette", palette),
                ("Data", Value::ByteArray(fastnbt::ByteArray::new(data))),
                ("BlockEntities", Value::List(vec![])),
            ]),
        ),
    ]);
    gzip(&fastnbt::to_bytes(&compound(vec![("Schematic", schem)])).unwrap())
}

/// The same blocks as a vanilla structure file (air left out, as structure blocks may).
fn structure_bytes() -> Vec<u8> {
    let mut palette = Vec::new();
    let mut blocks = Vec::new();
    for (i, (p, s)) in test_blocks().into_iter().enumerate() {
        let b = BlockState::parse(s).unwrap();
        let mut entry = vec![("Name", Value::String(b.name.clone()))];
        if !b.properties.is_empty() {
            entry.push((
                "Properties",
                Value::Compound(
                    b.properties
                        .iter()
                        .map(|(k, v)| (k.clone(), Value::String(v.clone())))
                        .collect(),
                ),
            ));
        }
        palette.push(compound(entry));
        blocks.push(compound(vec![
            ("state", Value::Int(i as i32)),
            ("pos", Value::List(p.iter().map(|&c| Value::Int(c)).collect())),
        ]));
    }
    let root = compound(vec![
        ("DataVersion", Value::Int(5023)),
        ("size", Value::List(vec![Value::Int(3), Value::Int(2), Value::Int(2)])),
        ("palette", Value::List(palette)),
        ("blocks", Value::List(blocks)),
        ("entities", Value::List(vec![])),
    ]);
    gzip(&fastnbt::to_bytes(&root).unwrap())
}

fn sorted(s: &Schematic) -> Vec<([i32; 3], String)> {
    let mut v: Vec<_> =
        s.blocks.iter().map(|(p, i)| (*p, s.palette[*i as usize].to_string())).collect();
    v.sort();
    v
}

#[test]
fn both_formats_read_the_same_blocks() {
    let dir = std::env::temp_dir().join(format!("hhmc-landmarks-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("t.schem"), sponge_bytes()).unwrap();
    std::fs::write(dir.join("t.nbt"), structure_bytes()).unwrap();
    let a = load_schematic(&dir.join("t.schem")).unwrap();
    let b = load_schematic(&dir.join("t.nbt")).unwrap();
    assert_eq!(a.size, [3, 2, 2]);
    assert_eq!(a.blocks.len(), 4, "air places nothing");
    assert_eq!(sorted(&a), sorted(&b));
    let mut want: Vec<_> = test_blocks()
        .into_iter()
        .map(|(p, s)| (p, BlockState::parse(s).unwrap().to_string()))
        .collect();
    want.sort();
    assert_eq!(sorted(&a), want);
}

#[test]
fn block_states_turn_with_the_landmark() {
    let r = |s: &str, q| rotate_state(&BlockState::parse(s).unwrap(), q).to_string();
    let p = |s: &str| BlockState::parse(s).unwrap().to_string();
    assert_eq!(
        r("minecraft:oak_stairs[facing=north,shape=inner_left]", 1),
        p("minecraft:oak_stairs[facing=east,shape=inner_left]")
    );
    assert_eq!(r("minecraft:oak_log[axis=x]", 1), p("minecraft:oak_log[axis=z]"));
    assert_eq!(r("minecraft:oak_log[axis=x]", 2), p("minecraft:oak_log[axis=x]"));
    assert_eq!(
        r("minecraft:oak_fence[east=true,north=false,south=false,west=false]", 1),
        p("minecraft:oak_fence[east=false,north=false,south=true,west=false]")
    );
    assert_eq!(r("minecraft:oak_sign[rotation=3]", 1), p("minecraft:oak_sign[rotation=7]"));
    assert_eq!(r("minecraft:rail[shape=north_south]", 1), p("minecraft:rail[shape=east_west]"));
    assert_eq!(r("minecraft:rail[shape=south_east]", 1), p("minecraft:rail[shape=south_west]"));
    assert_eq!(r("minecraft:rail[shape=north_east]", 2), p("minecraft:rail[shape=south_west]"));
    assert_eq!(
        r("minecraft:rail[shape=ascending_north]", 3),
        p("minecraft:rail[shape=ascending_west]")
    );
    assert_eq!(
        r("minecraft:cobblestone_wall[east=tall,north=none,south=low,west=none]", 3),
        p("minecraft:cobblestone_wall[east=low,north=tall,south=none,west=none]")
    );
}

/// Flat ground at y 5 for tile `t`.
fn flat(t: TileKey) -> GroundModel {
    let w = TILE_SIZE as usize + 2 * TILE_BUFFER as usize;
    let (bx, bz) = t.min_block();
    GroundModel {
        key: t,
        min_e: (O.e0 + i64::from(bx)) as f64 - TILE_BUFFER,
        max_n: (O.n0 - i64::from(bz)) as f64 + TILE_BUFFER,
        w,
        h: w,
        ground: vec![5; w * w],
        class: vec![2; w * w],
        water_top: vec![NO_WATER; w * w],
        body: vec![0; w * w],
        stats: GroundStats::default(),
    }
}

fn name_at(t: &VoxelTile, x: i32, y: i32, z: i32) -> String {
    let (bx, bz) = t.key.min_block();
    t.palette.get(t.get((x - bx) as usize, y, (z - bz) as usize)).to_string()
}

#[test]
fn landmark_is_placed_rotated_and_cleared_across_a_tile_boundary() {
    let root = std::env::temp_dir().join(format!("hhmc-landmarks-place-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("test.schem"), sponge_bytes()).unwrap();
    // Anchor at world block (512, 6, 100), turned a quarter clockwise: the schematic's x
    // runs south, its z west, so its two z rows land in x 512 (tile 1, 0) and 511 (tile 0, 0).
    // The footprint spans x 509..=514, z 98..=104 over both tiles.
    let l = Landmark {
        name: "test".into(),
        schematic: "test.schem".into(),
        anchor: [512.5, 1000.0 - 100.5, 6.0],
        origin: [0, 0, 0],
        rotation: 90,
        footprint: vec![[509.0, 902.0], [515.0, 902.0], [515.0, 895.0], [509.0, 895.0]],
    };
    let placed = vec![place(&root, &l, &O, &H).unwrap()];
    let blocks_cfg: BlocksConfig = hhmc_core::config::load_toml(
        &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config/blocks.toml"),
    )
    .unwrap();
    let mut tiles = Vec::new();
    for t in [TileKey::new(0, 0), TileKey::new(1, 0)] {
        let g = flat(t);
        let mut vt = hhmc_world::compose::compose_ground(&g, &blocks_cfg).unwrap();
        // Generated blocks: building columns inside the footprint in both tiles, and one
        // outside it.
        let (bx, bz) = t.min_block();
        let mut gen_blocks = Vec::new();
        for (x, z) in [(510, 99), (513, 103), (520, 100)] {
            if (x - bx) as u32 >= TILE_SIZE as u32 {
                continue;
            }
            for y in 6..=10 {
                gen_blocks.push(((x - bx) as u16, y as i16, (z - bz) as u16, 0u16));
            }
        }
        hhmc_world::compose::apply_layer(
            &mut vt,
            &[BlockState::new("stone_bricks")],
            &[],
            &gen_blocks,
        );
        let layer = landmark_layer(&placed, &g, &O);
        assert_eq!(layer.names, vec!["test".to_string()], "tile {t}");
        hhmc_world::compose::apply_layer(&mut vt, &layer.palette, &layer.clears, &layer.blocks);
        tiles.push(vt);
    }
    let (t0, t1) = (&tiles[0], &tiles[1]);
    let p = |s: &str| BlockState::parse(s).unwrap().to_string();
    // Placed and turned: local (x, y, z) → world (512 − z, 6 + y, 100 + x).
    assert_eq!(name_at(t1, 512, 6, 100), p("minecraft:stone"));
    assert_eq!(
        name_at(t1, 512, 6, 101),
        p("minecraft:oak_stairs[facing=east,half=bottom,shape=straight,waterlogged=false]")
    );
    assert_eq!(name_at(t1, 512, 6, 102), p("minecraft:oak_log[axis=z]"));
    assert_eq!(
        name_at(t0, 511, 7, 100),
        p("minecraft:oak_fence[east=false,north=false,south=true,waterlogged=false,west=false]")
    );
    // Footprint cleared above the ground in both tiles; the ground stays.
    for (t, x, z) in [(t0, 510, 99), (t1, 513, 103)] {
        for y in 6..=10 {
            assert_eq!(name_at(t, x, y, z), "minecraft:air", "{x} {y} {z}");
        }
        assert_ne!(name_at(t, x, 5, z), "minecraft:air");
    }
    // Outside the footprint the generated blocks stay.
    assert_eq!(name_at(t1, 520, 8, 100), "minecraft:stone_bricks");
    // Schematic air placed nothing outside the footprint's clearing (the fence's
    // neighbors in the schematic are air, the column below it is cleared ground + 1).
    assert_eq!(name_at(t0, 511, 6, 100), "minecraft:air");
}
