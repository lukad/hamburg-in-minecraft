//! Synthetic scenes: flat land at y = 5 with a canal (bed y = 0, water to y = 2) along
//! x 100..110 of tile (0, 0); origin E0 = 0, N0 = 1000 (block z = 999 − N for centers).

use hhmc_core::geom::Bbox;
use hhmc_core::ground::GroundStats;
use hhmc_core::infra::{Deck, Pier, Rings, Track};
use hhmc_core::{TILE_BUFFER, TileKey};

use super::*;

const O: Origin = Origin { e0: 0, n0: 1000 };
const H: HeightMapping = HeightMapping { min_y: -64, max_y: 319, clamp_min: -63, clamp_max: 318 };

fn cfg() -> (InfraConfig, BlocksConfig) {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config");
    (
        hhmc_core::config::load_toml(&root.join("infra.toml")).unwrap(),
        hhmc_core::config::load_toml(&root.join("blocks.toml")).unwrap(),
    )
}

fn ground() -> GroundModel {
    let w = TILE_SIZE as usize + 2 * TILE_BUFFER as usize;
    let mut g = GroundModel {
        key: TileKey::new(0, 0),
        min_e: -TILE_BUFFER,
        max_n: 1000.0 + TILE_BUFFER,
        w,
        h: w,
        ground: vec![5; w * w],
        class: vec![2; w * w], // road
        water_top: vec![NO_WATER; w * w],
        body: vec![0; w * w],
        stats: GroundStats::default(),
    };
    for z in 0..512 {
        for x in 100..110 {
            let i = g.idx(x, z);
            g.ground[i] = 0;
            g.water_top[i] = 2;
            g.class[i] = 8; // water
        }
    }
    g
}

/// bDOM: `deck` m inside `area`, the ground elsewhere.
fn bdom(area: &Bbox, deck: f32) -> Grid<f32> {
    let b = Bbox::new(
        -TILE_BUFFER,
        1000.0 - 512.0 - TILE_BUFFER,
        512.0 + TILE_BUFFER,
        1000.0 + TILE_BUFFER,
    );
    let mut g = Grid::filled(&b, 5.0f32);
    for r in 0..g.h {
        for c in 0..g.w {
            let (e, n) = (g.min_e + c as f64 + 0.5, g.max_n - r as f64 - 0.5);
            if area.contains(e, n) {
                g.set(c, r, deck);
            } else if (100.0..110.0).contains(&e) {
                g.set(c, r, 2.0);
            }
        }
    }
    g
}

fn rect(e0: f64, n0: f64, e1: f64, n1: f64) -> Rings {
    vec![vec![(e0, n0), (e1, n0), (e1, n1), (e0, n1), (e0, n0)]]
}

fn run(s: &TileInfra, bdom: &Grid<f32>) -> InfraLayer {
    let (c, b) = cfg();
    infra(&InfraInputs {
        structures: s,
        cfg: &c,
        blocks: &b,
        origin: &O,
        height: &H,
        ground: &ground(),
        bdom,
        dgm: None,
        alkis_class: None,
    })
    .unwrap()
}

fn block(l: &InfraLayer, x: u16, y: i16, z: u16) -> Option<&BlockState> {
    l.blocks.iter().find(|b| (b.0, b.1, b.2) == (x, y, z)).map(|b| &l.palette[b.3 as usize])
}

/// A road bridge over the canal (E 96..114, N 490..498 → z 502..509) at 8 m.
fn bridge() -> Deck {
    let (c, _) = cfg();
    Deck {
        id: "B".into(),
        kind: c.deck_kind(1800).unwrap(),
        polygons: vec![rect(96.0, 490.0, 114.0, 498.0)],
    }
}

#[test]
fn bridge_spans_the_canal_with_clearance() {
    let s = TileInfra { decks: vec![bridge()], ..Default::default() };
    let l = run(&s, &bdom(&Bbox::new(96.0, 490.0, 114.0, 498.0), 10.0));
    // Deck top at 10 over the water, structure at 9, water and air below untouched.
    assert_eq!(block(&l, 105, 10, 505).unwrap().name.as_str(), "minecraft:gray_concrete");
    assert_eq!(block(&l, 105, 9, 505).unwrap().name.as_str(), "minecraft:stone_bricks");
    for y in 0..=6 {
        assert!(block(&l, 105, y, 505).is_none(), "y {y}");
    }
    // Railings on the long sides over the water, none where the deck meets the road.
    let r = block(&l, 105, 11, 502).unwrap();
    assert_eq!(r.name.as_str(), "minecraft:iron_bars");
    assert_eq!(r.properties.get("east").map(String::as_str), Some("true"));
    // At the ends the deck is 5 blocks above the road here: a drop, so a railing.
    assert!(block(&l, 96, 11, 505).is_some());
    assert_eq!(l.stats.raised_for_clearance, 0);
}

#[test]
fn low_bridge_is_raised_above_the_water() {
    let s = TileInfra { decks: vec![bridge()], ..Default::default() };
    let l = run(&s, &bdom(&Bbox::new(96.0, 490.0, 114.0, 498.0), 2.0));
    // Water to 2, one block of air, two deck blocks: top at 5.
    let d = l.decks.iter().find(|d| (d.x, d.z) == (105, 505)).unwrap();
    assert_eq!((d.top, d.bottom), (5, 4));
    assert!(l.stats.raised_for_clearance > 0);
}

#[test]
fn landing_stage_floats_and_pier_meets_the_deck() {
    let (c, _) = cfg();
    let pontoon = Deck {
        id: "L".into(),
        kind: c.deck_kind(1810).unwrap(),
        polygons: vec![rect(101.0, 600.0, 108.0, 620.0)],
    };
    let pier =
        Pier { id: "P".into(), generated: false, polygons: vec![rect(104.0, 493.0, 106.0, 495.0)] };
    let s = TileInfra { decks: vec![bridge(), pontoon], piers: vec![pier], ..Default::default() };
    let l = run(&s, &bdom(&Bbox::new(96.0, 490.0, 114.0, 498.0), 10.0));
    // Pontoon: top at water + 1 = 3, hull at the water surface 2.
    let p = l.decks.iter().find(|d| (d.x, d.z) == (104, 390)).unwrap();
    assert_eq!((p.top, p.bottom), (3, 2));
    assert!(l.floating_tops(&c)[390 * 512 + 104] == 3);
    // Pier from the bed up to the deck's underside (7 − 1).
    assert_eq!(l.piers.iter().find(|q| (q.0, q.1) == (104, 505)), Some(&(104, 505, 1, 8)));
}

#[test]
fn rails_follow_the_deck() {
    let t = Track {
        id: "T".into(),
        line: (0..=40).map(|k| (90.5 + f64::from(k) * 0.75, 494.5)).collect(),
        portal_start: false,
        portal_end: false,
    };
    let s = TileInfra { decks: vec![bridge()], tracks: vec![t], ..Default::default() };
    let l = run(&s, &bdom(&Bbox::new(96.0, 490.0, 114.0, 498.0), 10.0));
    // On the deck at 9, on the road at 6, ascending where the deck starts.
    assert_eq!(
        block(&l, 105, 11, 505).unwrap().properties.get("shape").map(String::as_str),
        Some("east_west")
    );
    assert_eq!(
        block(&l, 92, 6, 505).unwrap().properties.get("shape").map(String::as_str),
        Some("east_west")
    );
    let path = &l.tracks[0].1;
    assert!(path.iter().any(|c| c.deck.is_some()) && path.iter().any(|c| c.deck.is_none()));
}

#[test]
fn rail_shapes() {
    let s = 5;
    assert_eq!(
        rail_shape((0, 0), s, Some(((0, -1), Some(5))), Some(((0, 1), Some(5)))),
        "north_south"
    );
    assert_eq!(
        rail_shape((0, 0), s, Some(((-1, 0), Some(5))), Some(((1, 0), Some(6)))),
        "ascending_east"
    );
    assert_eq!(
        rail_shape((0, 0), s, Some(((0, 1), Some(5))), Some(((1, 0), Some(5)))),
        "south_east"
    );
    assert_eq!(grid_path(&[(0.5, 999.5), (1.5, 998.5)], &O), vec![(0, 0), (1, 0), (1, 1)]);
}

#[test]
fn gangway_lands_on_the_pontoon() {
    let (c, _) = cfg();
    // Pontoon in the canal (E 101..109, N 600..620), gangway from the bank at E 96 onto it
    // at its west end, low in bDOM (taken at low tide).
    let pontoon = Deck {
        id: "L".into(),
        kind: c.deck_kind(1810).unwrap(),
        polygons: vec![rect(101.0, 600.0, 109.0, 620.0)],
    };
    let gangway = Deck {
        id: "G".into(),
        kind: c.deck_kind(1800).unwrap(),
        polygons: vec![rect(96.0, 608.0, 104.0, 611.0)],
    };
    let s = TileInfra { decks: vec![pontoon, gangway], ..Default::default() };
    let l = run(&s, &bdom(&Bbox::new(96.0, 608.0, 104.0, 611.0), 1.5));
    // Over open water: just above it (no clearance), no extra air demanded.
    let w = l.decks.iter().find(|d| (d.x, d.z) == (100, 390) && d.kind == 0).unwrap();
    // (Its quay end is pinned to the quay at 5, so it slopes down toward the pontoon.)
    assert!(w.bottom >= 3 && w.clearance == 0, "{w:?}");
    // On the pontoon: at most one block above its top (a step onto it).
    assert!(l.decks.iter().all(|d| !((d.x, d.z) == (102, 390) && d.kind == 0) || d.top <= 4));
    assert_eq!(block(&l, 102, 3, 390).unwrap().name.as_str(), "minecraft:polished_andesite");
}

#[test]
fn railway_passes_through_stations() {
    let (c, _) = cfg();
    let v = Deck {
        id: "V".into(),
        kind: c.deck_kind(1830).unwrap(),
        polygons: vec![rect(20.0, 700.0, 80.0, 708.0)],
    };
    // A station building over the middle of the viaduct: its roof (15 m) must not lift
    // the deck (10 m).
    let station = rect(40.0, 698.0, 60.0, 710.0);
    let mut b = bdom(&Bbox::new(20.0, 700.0, 80.0, 708.0), 10.0);
    for r in 0..b.h {
        for col in 0..b.w {
            let (e, n) = (b.min_e + col as f64 + 0.5, b.max_n - r as f64 - 0.5);
            if (40.0..60.0).contains(&e) && (698.0..710.0).contains(&n) {
                b.set(col, r, 15.0);
            }
        }
    }
    let t = Track {
        id: "T".into(),
        line: (0..=112).map(|k| (22.0 + f64::from(k) * 0.5, 704.5)).collect(),
        portal_start: false,
        portal_end: false,
    };
    let s = TileInfra {
        decks: vec![v],
        occluders: vec![station],
        tracks: vec![t],
        ..Default::default()
    };
    let l = run(&s, &b);
    let d = l.decks.iter().find(|d| (d.x, d.z) == (50, 295)).unwrap();
    assert_eq!(d.top, 10);
    // Overrides along the track: the column from the underside to 4 above the deck.
    assert!(l.override_clears.contains(&(50, 295, 9, 14)));
    assert!(l.override_blocks.iter().any(|b| (b.0, b.1, b.2) == (50, 10, 295)));
}

#[test]
fn bridge_ends_meet_the_street() {
    // The deck rises from the street (5) at its ends to 6.8 m over the middle; the height
    // smoothing lifts its ends, the street they land on brings them back down.
    let s = TileInfra { decks: vec![bridge()], ..Default::default() };
    let mut bd = bdom(&Bbox::new(96.0, 490.0, 114.0, 498.0), 6.8);
    for r in 0..bd.h {
        for c in 0..bd.w {
            let (e, n) = (bd.min_e + c as f64 + 0.5, bd.max_n - r as f64 - 0.5);
            if (96.0..114.0).contains(&e) && (490.0..498.0).contains(&n) {
                bd.set(c, r, 5.0 + 1.8 * (1.0 - (e - 105.0).abs() / 9.0) as f32);
            }
        }
    }
    let l = run(&s, &bd);
    for c in &l.street_contacts {
        assert!((c.2 - c.3).abs() <= 1, "deck {} vs street {} at {} {}", c.2, c.3, c.0, c.1);
    }
    // No railing across the road at the ends.
    assert!((94..=99).all(|x| {
        (0..20).all(|y| block(&l, x, y, 505).is_none_or(|b| !b.name.ends_with("iron_bars")))
    }));
    // Over the water: above it with clearance.
    let mid = l.decks.iter().find(|d| (d.x, d.z) == (105, 505)).unwrap();
    assert!(mid.bottom >= 4, "{mid:?}");
}

#[test]
fn railings_close_diagonal_corners() {
    // A bridge running at 45° over the canal.
    let r = vec![vec![(92.0, 490.0), (96.0, 486.0), (118.0, 508.0), (114.0, 512.0), (92.0, 490.0)]];
    let (c, _) = cfg();
    let d = Deck { id: "D".into(), kind: c.deck_kind(1800).unwrap(), polygons: vec![r] };
    let s = TileInfra { decks: vec![d], ..Default::default() };
    let b = bdom(&Bbox::new(90.0, 484.0, 120.0, 514.0), 9.0);
    let l = run(&s, &b);
    let rail: std::collections::HashSet<(u16, i16, u16)> = l
        .blocks
        .iter()
        .filter(|b| l.palette[b.3 as usize].name.ends_with("iron_bars"))
        .map(|b| (b.0, b.1, b.2))
        .collect();
    assert!(rail.len() > 10);
    for &(x, y, z) in &rail {
        for dz in [1i32, -1] {
            let nz = (i32::from(z) + dz) as u16;
            if rail.contains(&(x + 1, y, nz)) {
                assert!(rail.contains(&(x + 1, y, z)) || rail.contains(&(x, y, nz)), "{x} {y} {z}");
            }
        }
    }
}

#[test]
fn tracks_bridge_gaps_between_decks_and_level_their_bed() {
    let (c, _) = cfg();
    // Two viaduct polygons (E 20..40 and 52..80, at 12 m) with a 12 m gap over the road,
    // and a track along them that continues on the ground to the east.
    let k = c.deck_kind(1830).unwrap();
    let a = Deck { id: "A".into(), kind: k, polygons: vec![rect(20.0, 700.0, 40.0, 708.0)] };
    let b = Deck { id: "B".into(), kind: k, polygons: vec![rect(52.0, 700.0, 80.0, 708.0)] };
    let t = Track {
        id: "T".into(),
        line: (0..=200).map(|i| (21.0 + f64::from(i) * 0.5, 704.5)).collect(),
        portal_start: false,
        portal_end: false,
    };
    let s = TileInfra { decks: vec![a, b], tracks: vec![t], ..Default::default() };
    let mut bd = bdom(&Bbox::new(20.0, 700.0, 40.0, 708.0), 12.0);
    let b2 = bdom(&Bbox::new(52.0, 700.0, 80.0, 708.0), 12.0);
    for (v, w) in bd.data.iter_mut().zip(&b2.data) {
        *v = v.max(*w);
    }
    // A pit in the ground under the track east of the viaduct.
    let mut gm = ground();
    let i = gm.idx(90, 295);
    gm.ground[i] = 4;
    let (cf, bl) = cfg();
    let l = infra(&InfraInputs {
        structures: &s,
        cfg: &cf,
        blocks: &bl,
        origin: &O,
        height: &H,
        ground: &gm,
        bdom: &bd,
        dgm: None,
        alkis_class: None,
    })
    .unwrap();
    // Rails continuous at 13 across the gap, carried by a gap deck.
    for x in 21..80u16 {
        assert_eq!(block(&l, x, 13, 295).map(|b| b.name.as_str()), Some("minecraft:rail"), "x {x}");
    }
    assert!(l.stats.gap_deck_cells > 0);
    // The pit is filled: the rail stays at 6.
    assert_eq!(block(&l, 90, 6, 295).map(|b| b.name.as_str()), Some("minecraft:rail"));
    assert_eq!(block(&l, 90, 5, 295).map(|b| b.name.as_str()), Some("minecraft:gravel"));
}

#[test]
fn piers_without_a_deck_are_not_drawn() {
    let pier =
        Pier { id: "P".into(), generated: false, polygons: vec![rect(104.0, 400.0, 106.0, 402.0)] };
    let s = TileInfra { piers: vec![pier], ..Default::default() };
    let l = run(&s, &bdom(&Bbox::new(0.0, 0.0, 1.0, 1.0), 30.0));
    assert!(l.piers.is_empty() && l.blocks.is_empty());
}

#[test]
fn bridges_reach_the_banks_and_carry_the_lanes_across() {
    // Streets on both banks: road at z 502..504, sidewalk at z 505..507 (N 492..498).
    let mut gm = ground();
    for z in 505..=507 {
        for x in 60..150 {
            let i = gm.idx(x, z);
            if gm.water_top[i] == NO_WATER {
                gm.class[i] = 3; // sidewalk
            }
        }
    }
    // The bridge polygon stops 2 m short of both banks (E 102..108 over the canal at
    // x 100..110), across the road and the sidewalk (N 492..498 → z 502..507).
    let (c, b) = cfg();
    let d = Deck {
        id: "B".into(),
        kind: c.deck_kind(1800).unwrap(),
        polygons: vec![rect(102.0, 492.0, 108.0, 498.0)],
    };
    let s = TileInfra { decks: vec![d], ..Default::default() };
    let bd = bdom(&Bbox::new(102.0, 492.0, 108.0, 498.0), 5.4);
    let l = infra(&InfraInputs {
        structures: &s,
        cfg: &c,
        blocks: &b,
        origin: &O,
        height: &H,
        ground: &gm,
        bdom: &bd,
        dgm: None,
        alkis_class: None,
    })
    .unwrap();
    // Extended to both banks, at street level.
    for x in 100..110u16 {
        let d = l.decks.iter().find(|d| (d.x, d.z) == (x, 503)).unwrap_or_else(|| panic!("x {x}"));
        assert_eq!(d.top, 5, "x {x}");
    }
    // Road and sidewalk continue straight across.
    for x in 100..110u16 {
        assert_eq!(block(&l, x, 5, 503).unwrap().name.as_str(), "minecraft:gray_concrete", "x {x}");
        assert_eq!(block(&l, x, 5, 506).unwrap().name.as_str(), "minecraft:smooth_stone", "x {x}");
    }
}

#[test]
fn bridges_run_on_over_abutments_where_the_terrain_dips() {
    // DGM1 has no bridges: beside the canal the ground dips from the street (5) to 1 at
    // x 96..99 and 110..113. Beside the bridge (z 502..509) the street (bDOM 5.2) runs on
    // at its height; elsewhere bDOM sees the dip.
    let mut gm = ground();
    let dip: [(usize, i16); 8] =
        [(96, 4), (97, 3), (98, 2), (99, 1), (110, 1), (111, 2), (112, 3), (113, 4)];
    for z in 0..512 {
        for &(x, y) in &dip {
            let i = gm.idx(x, z);
            gm.ground[i] = y;
            gm.water_top[i] = NO_WATER;
            gm.class[i] = 2;
        }
    }
    let (c, b) = cfg();
    let d = Deck {
        id: "B".into(),
        kind: c.deck_kind(1800).unwrap(),
        polygons: vec![rect(100.0, 490.0, 110.0, 498.0)],
    };
    let s = TileInfra { decks: vec![d], ..Default::default() };
    let mut bd = bdom(&Bbox::new(90.0, 490.0, 120.0, 498.0), 5.2);
    for r in 0..bd.h {
        for col in 0..bd.w {
            let (e, n) = (bd.min_e + col as f64 + 0.5, bd.max_n - r as f64 - 0.5);
            if let Some(&(_, y)) = dip.iter().find(|d| d.0 as f64 + 0.5 == e)
                && !(490.0..498.0).contains(&n)
            {
                bd.set(col, r, f32::from(y));
            }
        }
    }
    let l = infra(&InfraInputs {
        structures: &s,
        cfg: &c,
        blocks: &b,
        origin: &O,
        height: &H,
        ground: &gm,
        bdom: &bd,
        dgm: None,
        alkis_class: None,
    })
    .unwrap();
    // The deck runs on over the dip to the street, level with it, in the road's surface.
    for x in 96..114u16 {
        let d = l.decks.iter().find(|d| (d.x, d.z) == (x, 505)).unwrap_or_else(|| panic!("x {x}"));
        assert_eq!(d.top, 5, "x {x}");
    }
    assert_eq!(block(&l, 97, 5, 505).unwrap().name.as_str(), "minecraft:gray_concrete");
    // Only beside the bridge: the bank dips elsewhere.
    assert!(!l.decks.iter().any(|d| d.z < 500 || d.z > 511));
    assert!(l.stats.abutment_cells > 0);
}

#[test]
fn railings_leave_narrow_decks_walkable() {
    // A footbridge three cells wide over water: rows z 0..10
    // at x 5..=7, rows 10..20 one cell further west, with railings on both edges. The
    // edge railings meet diagonally at the step; the bank (land at the deck's height) is
    // north of row 0.
    let cells = Cells { w: 20, h: 22, border: 0 };
    let mut dcells = Vec::new();
    let mut railing: BTreeMap<(i32, i32, i32), usize> = BTreeMap::new();
    for z in 1..21i64 {
        let x0 = if z < 11 { 5 } else { 4 };
        for x in x0..x0 + 3 {
            let cell = cells.at(x, z).unwrap();
            dcells.push(DeckCell { kind: 0, cell, top: 8, bottom: 7, clearance: 1 });
            if x != x0 + 1 {
                railing.insert((x as i32, 9, z as i32), 0);
            }
        }
    }
    let mut at: HashMap<usize, Vec<usize>> = HashMap::new();
    for (k, d) in dcells.iter().enumerate() {
        at.entry(d.cell).or_default().push(k);
    }
    let land = |n: usize| (n / cells.w == 0).then_some(8);
    open_walkways(&cells, &dcells, &at, &mut railing, &land);
    let walk: Vec<(i64, i64)> = dcells
        .iter()
        .map(|d| cells.col_row(d.cell))
        .filter(|&(x, z)| !railing.contains_key(&(x as i32, 9, z as i32)))
        .collect();
    let mut seen = vec![(6, 1)];
    let mut k = 0;
    while k < seen.len() {
        let (x, z) = seen[k];
        k += 1;
        for n in [(x + 1, z), (x - 1, z), (x, z + 1), (x, z - 1)] {
            if walk.contains(&n) && !seen.contains(&n) {
                seen.push(n);
            }
        }
    }
    assert!(seen.contains(&(5, 20)), "the far end is cut off: {seen:?}");
    // Only the step is opened: the edges keep their railings elsewhere.
    assert!(railing.len() >= 2 * 20 - 3, "{}", railing.len());
}
