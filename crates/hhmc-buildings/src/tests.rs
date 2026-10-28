//! Golden tests: flat box, gable roof, hip roof, flat roof with a
//! courtyard, stacked building parts. Origin E0 = 0, N0 = 100, so block x = E and
//! z = 99 − N for column centers; the ground is at y = 0 everywhere.

use hhmc_citygml::{Building, BuildingMesh, Part, Polygon, SurfaceKind};
use hhmc_core::config::Origin;
use hhmc_core::ground::{GroundModel, GroundStats, NO_WATER};
use hhmc_core::landcfg::BuildingStyle;
use hhmc_core::{BlockState, TileKey};

use super::*;

const O: Origin = Origin { e0: 0, n0: 100 };

fn style() -> BuildingStyle {
    BuildingStyle {
        wall: "minecraft:bricks".into(),
        floor: "minecraft:oak_planks".into(),
        flat_roof: "minecraft:smooth_stone".into(),
        flat_roof_slab: "minecraft:smooth_stone_slab".into(),
        roof: "minecraft:deepslate_tiles".into(),
        roof_slab: "minecraft:deepslate_tile_slab".into(),
        roof_stairs: "minecraft:deepslate_tile_stairs".into(),
        flat_max_deg: 10.0,
        stairs_min_deg: 30.0,
        stairs_max_deg: 60.0,
        story_blocks: 3,
        foundation_max_blocks: 3,
        canopy_functions: vec!["51009_1610".into()],
        canopy_post_spacing: 6,
    }
}

fn ground() -> GroundModel {
    let n = 576;
    GroundModel {
        key: TileKey::new(0, 0),
        min_e: -32.0,
        max_n: 132.0,
        w: n,
        h: n,
        ground: vec![0; n * n],
        class: vec![0; n * n],
        water_top: vec![NO_WATER; n * n],
        body: vec![0; n * n],
        stats: GroundStats::default(),
    }
}

fn poly(kind: SurfaceKind, rings: Vec<Vec<[f64; 3]>>) -> Polygon {
    Polygon { id: None, ring_ids: vec![None; rings.len()], kind, lod: 2, thematic: true, rings }
}

/// Vertical walls along a closed footprint ring from height `lo` to the roof height
/// given per vertex.
fn walls(ring: &[[f64; 2]], lo: f64, hi: &dyn Fn([f64; 2]) -> f64) -> Vec<Polygon> {
    (0..ring.len())
        .map(|i| {
            let (a, b) = (ring[i], ring[(i + 1) % ring.len()]);
            poly(
                SurfaceKind::Wall,
                vec![vec![
                    [a[0], a[1], lo],
                    [b[0], b[1], lo],
                    [b[0], b[1], hi(b)],
                    [a[0], a[1], hi(a)],
                ]],
            )
        })
        .collect()
}

fn flat(rings: &[Vec<[f64; 2]>], h: f64, kind: SurfaceKind) -> Polygon {
    poly(kind, rings.iter().map(|r| r.iter().map(|p| [p[0], p[1], h]).collect()).collect())
}

fn rect(e0: f64, n0: f64, e1: f64, n1: f64) -> Vec<[f64; 2]> {
    vec![[e0, n0], [e1, n0], [e1, n1], [e0, n1]]
}

fn bx(e0: f64, n0: f64, e1: f64, n1: f64, lo: f64, hi: f64) -> Vec<Polygon> {
    let r = rect(e0, n0, e1, n1);
    let mut v = walls(&r, lo, &|_| hi);
    v.push(flat(std::slice::from_ref(&r), lo, SurfaceKind::Ground));
    v.push(flat(&[r], hi, SurfaceKind::Roof));
    v
}

fn mesh(parts: Vec<Vec<Polygon>>) -> BuildingMesh {
    let b = Building {
        id: "T".into(),
        parts: parts
            .into_iter()
            .enumerate()
            .map(|(i, polygons)| Part { id: format!("p{i}"), polygons })
            .collect(),
        ..Default::default()
    };
    BuildingMesh::from_building(&b, "test").unwrap()
}

fn layer(m: &BuildingMesh) -> BuildingLayer {
    building_layer(
        std::slice::from_ref(m),
        TileKey::new(0, 0),
        &O,
        &ground(),
        &style(),
        None,
        None,
        None,
    )
}

fn block_at(l: &BuildingLayer, x: u16, y: i16, z: u16) -> Option<&BlockState> {
    l.blocks.iter().find(|b| (b.0, b.1, b.2) == (x, y, z)).map(|b| &l.palette[b.3 as usize])
}

fn top_at(l: &BuildingLayer, x: u16, z: u16) -> (i16, &BlockState) {
    let b = l.blocks.iter().filter(|b| (b.0, b.2) == (x, z)).max_by_key(|b| b.1).unwrap();
    (b.1, &l.palette[b.3 as usize])
}

#[test]
fn flat_box() {
    // 10 × 10 m, 0–6 m: roof at 6.0 is the middle of block 6, so the top is a slab.
    let l = layer(&mesh(vec![bx(10.0, 80.0, 20.0, 90.0, 0.0, 6.0)]));
    assert_eq!(l.stats[0].columns_in_tile, 100);
    assert_eq!(l.stats[0].odd_columns, 0);
    // y0: 100 (base) + y1,2,4,5: 36 walls each + y3: 36 walls + 64 floor + y6: 100 roof.
    assert_eq!(l.blocks.len(), 444);
    let (y, top) = top_at(&l, 15, 15);
    assert_eq!(y, 6);
    assert_eq!(top.to_string(), "minecraft:smooth_stone_slab[type=bottom,waterlogged=false]");
    assert_eq!(block_at(&l, 10, 2, 15).unwrap().to_string(), "minecraft:bricks");
    assert!(block_at(&l, 15, 2, 15).is_none(), "interior is hollow");
    assert_eq!(block_at(&l, 15, 3, 15).unwrap().to_string(), "minecraft:oak_planks");
}

#[test]
fn gable_roof() {
    // E 10–20, N 80–88; eaves at 4 m, ridge along E at N = 84, 8 m: 45° faces.
    let r = rect(10.0, 80.0, 20.0, 88.0);
    let h = |p: [f64; 2]| if (p[1] - 84.0).abs() < 1e-9 { 8.0 } else { 4.0 };
    let ring =
        vec![[10.0, 80.0], [20.0, 80.0], [20.0, 84.0], [20.0, 88.0], [10.0, 88.0], [10.0, 84.0]];
    let mut polys = walls(&ring, 0.0, &h);
    polys.push(flat(&[r], 0.0, SurfaceKind::Ground));
    polys.push(poly(
        SurfaceKind::Roof,
        vec![vec![[10.0, 80.0, 4.0], [20.0, 80.0, 4.0], [20.0, 84.0, 8.0], [10.0, 84.0, 8.0]]],
    ));
    polys.push(poly(
        SurfaceKind::Roof,
        vec![vec![[10.0, 84.0, 8.0], [20.0, 84.0, 8.0], [20.0, 88.0, 4.0], [10.0, 88.0, 4.0]]],
    ));
    let l = layer(&mesh(vec![polys]));
    assert_eq!(l.stats[0].odd_columns, 0);
    // South half (z = 99 − N > 15) rises to the north; north half rises to the south.
    let (y_s, s) = top_at(&l, 15, 18);
    let (_, n) = top_at(&l, 15, 13);
    assert!(s.to_string().contains("deepslate_tile_stairs[facing=north"), "{s}");
    assert!(n.to_string().contains("deepslate_tile_stairs[facing=south"), "{n}");
    // N = 81.5: height 5.5 lies at the bottom of block 6, so block 5 is the top.
    assert_eq!(y_s, 5);
    let (y_ridge, _) = top_at(&l, 15, 16);
    assert_eq!(y_ridge, 7);
}

#[test]
fn hip_roof() {
    // E 10–20, N 80–90, eaves 4 m, ridge from E 15–15 … a pyramid-like hip with ridge
    // E 14–16 at N = 85, 8 m.
    let r = rect(10.0, 80.0, 20.0, 90.0);
    let mut polys = walls(&r, 0.0, &|_| 4.0);
    polys.push(flat(&[r], 0.0, SurfaceKind::Ground));
    let (a, b, c, d) = ([10.0, 80.0, 4.0], [20.0, 80.0, 4.0], [20.0, 90.0, 4.0], [10.0, 90.0, 4.0]);
    let (r1, r2) = ([14.0, 85.0, 8.0], [16.0, 85.0, 8.0]);
    polys.push(poly(SurfaceKind::Roof, vec![vec![a, b, r2, r1]]));
    polys.push(poly(SurfaceKind::Roof, vec![vec![b, c, r2]]));
    polys.push(poly(SurfaceKind::Roof, vec![vec![c, d, r1, r2]]));
    polys.push(poly(SurfaceKind::Roof, vec![vec![d, a, r1]]));
    let l = layer(&mesh(vec![polys]));
    assert_eq!(l.stats[0].odd_columns, 0);
    let tops: Vec<String> = (10..20u16)
        .flat_map(|x| (10..20u16).map(move |z| (x, z)))
        .map(|(x, z)| top_at(&l, x, z).1.to_string())
        .collect();
    for f in ["north", "south", "east", "west"] {
        assert!(tops.iter().any(|t| t.contains(&format!("facing={f}"))), "no stairs facing {f}");
    }
}

#[test]
fn flat_roof_with_courtyard() {
    // 20 × 20 m block with a 10 × 10 m courtyard, 0–9 m.
    let outer = rect(10.0, 70.0, 30.0, 90.0);
    let inner = rect(15.0, 75.0, 25.0, 85.0);
    let mut polys = walls(&outer, 0.0, &|_| 9.0);
    polys.extend(walls(&inner, 0.0, &|_| 9.0));
    polys.push(flat(&[outer.clone(), inner.clone()], 0.0, SurfaceKind::Ground));
    polys.push(flat(&[outer, inner], 9.0, SurfaceKind::Roof));
    let l = layer(&mesh(vec![polys]));
    assert_eq!(l.stats[0].columns_in_tile, 300);
    assert_eq!(l.stats[0].odd_columns, 0);
    // Courtyard columns x 15–24, z 15–24 (N 75–85) are empty.
    assert!(!l.blocks.iter().any(|b| (15..25).contains(&b.0) && (15..25).contains(&b.2)));
    // The courtyard-facing wall is a shell.
    assert_eq!(block_at(&l, 14, 4, 20).unwrap().to_string(), "minecraft:bricks");
}

#[test]
fn stacked_parts() {
    // Part A: 10 × 10 m, 0–4 m. Part B on top: 6 × 6 m, 4–10 m.
    let l = layer(&mesh(vec![
        bx(10.0, 80.0, 20.0, 90.0, 0.0, 4.0),
        bx(12.0, 82.0, 18.0, 88.0, 4.0, 10.0),
    ]));
    // One continuous volume in B's footprint: no roof or floor where A's roof was, and no
    // shell between the parts.
    assert_eq!(top_at(&l, 15, 15).0, 10);
    assert!(block_at(&l, 15, 4, 15).is_none(), "no internal shell at the part boundary");
    // A's own roof outside B.
    let (y, a_roof) = top_at(&l, 10, 19);
    assert_eq!(y, 4);
    assert!(a_roof.to_string().starts_with("minecraft:smooth_stone_slab"));
}

#[test]
fn snapping_clips_and_founds() {
    // Model reaching 3 m below ground → starts at the ground; model floating 2 m above →
    // extended down; an overhang 6 m up stays.
    let mut v = Voxels::default();
    let iv =
        |bot, top| Interval { bot, top, top_kind: TopKind::Full, roof_normal: [0.0, 0.0, 1.0] };
    v.columns.insert((0, 0), vec![iv(-3, 5)]);
    v.columns.insert((1, 0), vec![iv(2, 5)]);
    v.columns.insert((2, 0), vec![iv(6, 9)]);
    snap_to_ground(&mut v, &|_, _| Some(0), 3);
    assert_eq!(v.columns[&(0, 0)][0].bot, 0);
    assert_eq!(v.columns[&(1, 0)][0].bot, 0);
    assert_eq!(v.columns[&(2, 0)][0].bot, 6);
}

#[test]
fn sub_block_building_gets_one_column() {
    // 0.6 × 0.6 m kiosk between column centers, 0–3 m.
    let l = layer(&mesh(vec![bx(10.6, 80.6, 11.2, 81.2, 0.0, 3.0)]));
    assert_eq!(l.stats[0].columns_in_tile, 1);
}

/// A closed base with a tower on top whose walls are single polygons (no thickness) and
/// whose cap is an installation, as in the LoD3 Rathaus.
fn open_tower(cap: bool) -> BuildingMesh {
    let mut p = bx(10.0, 10.0, 30.0, 30.0, 0.0, 10.0);
    p.extend(walls(&rect(15.0, 15.0, 25.0, 25.0), 10.0, &|_| 30.0));
    if cap {
        p.push(flat(&[rect(15.0, 15.0, 25.0, 25.0)], 30.0, SurfaceKind::Installation));
    }
    mesh(vec![p])
}

#[test]
fn tower_of_single_polygon_walls_is_closed() {
    let l = layer(&open_tower(true));
    // Walls on all four sides at mid height, corners included: the planes E 15 / 25 and
    // N 25 / 15 lie on block boundaries, the wall is the layer next to the interior.
    for z in 75..=85u16 {
        assert!(block_at(&l, 15, 20, z).is_some() && block_at(&l, 25, 20, z).is_some(), "z {z}");
    }
    for x in 15..=25u16 {
        assert!(block_at(&l, x, 20, 75).is_some() && block_at(&l, x, 20, 85).is_some(), "x {x}");
    }
    // The interior is filled solid (and hollowed like any volume): nothing inside at
    // mid height, the cap on top.
    assert!(block_at(&l, 20, 20, 80).is_none());
    assert_eq!(top_at(&l, 20, 80).0, 30);
}

#[test]
fn open_box_without_cap_stays_open() {
    // Without the cap the walls enclose nothing: no blocks above the base.
    let l = layer(&open_tower(false));
    assert!(l.blocks.iter().all(|b| b.1 <= 11), "{:?}", l.blocks.iter().map(|b| b.1).max());
}

#[test]
fn diagonal_walls_are_closed() {
    // A 1 m thick wall running at 45°, 20 m long, 8 m high.
    let r = vec![[10.0, 10.0], [10.7, 9.3], [24.7, 23.3], [24.0, 24.0]];
    let mut p = walls(&r, 0.0, &|_| 8.0);
    p.push(flat(std::slice::from_ref(&r), 0.0, SurfaceKind::Ground));
    p.push(flat(&[r], 8.0, SurfaceKind::Roof));
    let l = layer(&mesh(vec![p]));
    let at = |x: i32, z: i32| block_at(&l, x as u16, 4, z as u16).is_some();
    let cols: Vec<(i32, i32)> =
        l.blocks.iter().filter(|b| b.1 == 4).map(|b| (i32::from(b.0), i32::from(b.2))).collect();
    assert!(cols.len() > 10);
    for &(x, z) in &cols {
        for dz in [1, -1] {
            if at(x + 1, z + dz) {
                assert!(at(x + 1, z) || at(x, z + dz), "edge-only contact at {x} {z}");
            }
        }
    }
}

#[test]
fn triangle_box_overlap() {
    let t = [[0.0, 0.0, 0.0], [2.0, 0.0, 0.0], [0.0, 2.0, 0.0]];
    assert!(tri_box_overlap([0.5, 0.5, 0.4], 0.5, &t));
    assert!(!tri_box_overlap([0.5, 0.5, 0.6], 0.49, &t));
    // Beyond the hypotenuse.
    assert!(!tri_box_overlap([1.8, 1.8, 0.0], 0.3, &t));
    assert!(tri_box_overlap([1.2, 1.2, 0.0], 0.3, &t));
}
