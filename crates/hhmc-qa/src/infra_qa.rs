//! Infrastructure checks on the written region files:
//! - elevated railway: along every track on an elevated deck, each cell has the deck under
//!   a rail (also through stations), and neighboring cells differ by at most one block;
//! - bridges over water: the water under them is intact up to its level, with at least the
//!   deck's clearance of air above it (none for gangways resting on pontoons; pier,
//!   pontoon and building columns aside);
//! - floating decks (landing stages): the deck's top sits `freeboard` above the water and
//!   the hull fills the water's surface;
//! - decks meeting streets: steps of 2 or more blocks are counted for review (DGM1 blends
//!   quay walls and ramps into the cells beside a deck, so some remain);
//! - bridge decks over water continue the street surfaces they join (asphalt on asphalt,
//!   sidewalk on sidewalk), and every bridge deck has walkable joints at both ends;
//! - rails on the ground change height by at most one block between neighboring cells.

use hhmc_core::ground::{GroundModel, NO_WATER};
use hhmc_core::infra::InfraConfig;
use hhmc_core::{TILE_SIZE, TileKey, VoxelTile};
use hhmc_infra::InfraLayer;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct InfraQaResult {
    pub deck_columns: usize,
    pub elevated_track_cells: usize,
    /// Elevated track cells without deck and rail (or a building) in the world.
    pub elevated_gaps: usize,
    /// Neighboring elevated track cells more than one block apart in height.
    pub elevated_steps: usize,
    pub bridge_water_columns: usize,
    /// Bridge columns over water whose water or clearance is not intact.
    pub bridge_blocked: usize,
    /// Deck edges where the deck lands on land (the infra stage's landings: bare land
    /// continuing the deck's measured surface), those with a step of 2 or more blocks, and
    /// those with a step of one block.
    pub street_contacts: usize,
    pub street_steps: usize,
    #[serde(default)]
    pub street_rises: usize,
    /// Neighboring rail cells on the ground more than one block apart in height.
    pub ground_rail_cells: usize,
    pub ground_rail_steps: usize,
    /// Deck/street joints walkable (within a block) and those where the deck's surface
    /// block differs from the street's and from the street's within a cell of the joint
    /// (asphalt not lined up; a lane boundary crossing at an angle steps by a cell).
    pub surface_joints: usize,
    pub surface_mismatches: usize,
    /// Mismatched joints by "deck block -> street block".
    #[serde(default)]
    pub mismatch_pairs: std::collections::BTreeMap<String, usize>,
    /// World positions of the first mismatched deck cells.
    #[serde(default)]
    pub mismatch_samples: Vec<[i32; 3]>,
    /// Connected bridge decks (in the tile), those without any walkable joint (land within
    /// a block, or a building), and those whose joints are all at one end.
    pub bridge_decks: usize,
    pub isolated_decks: usize,
    pub one_sided_decks: usize,
    /// World (x, z) of a cell of each isolated or one-sided deck, with its finding.
    #[serde(default)]
    pub deck_findings: Vec<(String, [i32; 2])>,
    /// Adjacent columns of one deck kind whose tops differ by 2 blocks or more (a deck
    /// does not climb that fast: something above or below it was taken for the deck).
    #[serde(default)]
    pub deck_cliffs: usize,
    /// Coverage: ALKIS decks over water in this tile that produced no deck column.
    #[serde(default)]
    pub empty_decks: Vec<String>,
    pub floating_columns: usize,
    /// Floating deck columns not at water level + freeboard, or without their hull.
    pub floating_off_level: usize,
    /// World positions of the first offending columns.
    pub samples: Vec<[i32; 3]>,
    pub pass: bool,
}

pub struct InfraCheckInputs<'a> {
    pub key: TileKey,
    pub tile: &'a VoxelTile,
    pub ground: &'a GroundModel,
    pub infra: &'a InfraLayer,
    pub cfg: &'a InfraConfig,
    /// Highest building block per tile column, `i16::MIN` if none.
    pub building_top: &'a [i16],
    /// Deck/street height differences up to this many blocks count as the deck meeting
    /// the street (config/infra.toml `anchor_tolerance_m`).
    pub anchor_tolerance: i32,
    /// Land-use classes whose surface continues across decks.
    pub surface_classes: &'a [u8],
    /// Their top blocks (names), and the track ballast block.
    pub surface_tops: &'a [String],
    pub ballast: String,
}

pub fn check_infra(inp: &InfraCheckInputs) -> InfraQaResult {
    let (t, g, il) = (inp.tile, inp.ground, inp.infra);
    let size = TILE_SIZE as usize;
    let (bx, bz) = inp.key.min_block();
    let mut r = InfraQaResult { deck_columns: il.decks.len(), ..Default::default() };
    let name = |x: usize, y: i32, z: usize| t.palette.get(t.get(x, y, z)).name.clone();
    let is = |x: usize, y: i32, z: usize, n: &str| *name(x, y, z) == *n;
    let building = |x: usize, z: usize| inp.building_top[z * size + x] != i16::MIN;
    let sample = |r: &mut InfraQaResult, x: usize, y: i32, z: usize| {
        if r.samples.len() < 20 {
            r.samples.push([bx + x as i32, y, bz + z as i32]);
        }
    };
    let elevated: Vec<u8> = inp
        .cfg
        .deck
        .iter()
        .enumerate()
        .filter(|(_, d)| d.pass_through_clearance > 0)
        .map(|(i, _)| i as u8)
        .collect();
    for (_, cells) in &il.tracks {
        let mut prev: Option<(i32, i32, i16)> = None;
        for c in cells {
            if !c.deck.is_some_and(|k| elevated.contains(&k)) {
                prev = None;
                continue;
            }
            r.elevated_track_cells += 1;
            let (x, z) = ((c.x - bx) as usize, (c.z - bz) as usize);
            let y = i32::from(c.surface);
            // Railways pass through buildings (stations), so no exemption.
            let ok = !is(x, y, z, "minecraft:air") && is(x, y + 1, z, "minecraft:rail");
            if !ok {
                r.elevated_gaps += 1;
                sample(&mut r, x, y, z);
            }
            if let Some((px, pz, ps)) = prev
                && (px - c.x).abs() + (pz - c.z).abs() == 1
                && (ps - c.surface).abs() > 1
            {
                r.elevated_steps += 1;
                sample(&mut r, x, y, z);
            }
            prev = Some((c.x, c.z, c.surface));
        }
    }
    let mut step_squares: std::collections::BTreeSet<(i32, i32)> = Default::default();
    for &(x, z, top, street) in &il.street_contacts {
        let d = (i32::from(top) - i32::from(street)).abs();
        // Beyond the anchor tolerance the deck passes over the land (an overpass).
        if d > inp.anchor_tolerance {
            continue;
        }
        r.street_contacts += 1;
        if d >= 2 {
            r.street_steps += 1;
            step_squares.insert((i32::from(x).div_euclid(8), i32::from(z).div_euclid(8)));
        } else if d == 1 {
            r.street_rises += 1;
        }
    }
    for (cx, cz) in step_squares {
        r.deck_findings.push(("landing step".into(), [bx + cx * 8 + 4, bz + cz * 8 + 4]));
    }
    for (_, cells) in &il.tracks {
        let mut prev: Option<(i32, i32, i16)> = None;
        for c in cells {
            if c.deck.is_some() {
                prev = None;
                continue;
            }
            r.ground_rail_cells += 1;
            if let Some((px, pz, ps)) = prev
                && (px - c.x).abs() + (pz - c.z).abs() == 1
                && (ps - c.surface).abs() > 1
            {
                r.ground_rail_steps += 1;
                sample(&mut r, (c.x - bx) as usize, i32::from(c.surface), (c.z - bz) as usize);
            }
            prev = Some((c.x, c.z, c.surface));
        }
    }
    // Joints between bridge decks (kinds whose surface continues from the streets) and
    // streets, and the decks' connectivity.
    let size_i = TILE_SIZE;
    let continuing: Vec<u8> = inp
        .cfg
        .deck
        .iter()
        .enumerate()
        .filter(|(_, d)| d.surface_from_landuse && d.pass_through_clearance == 0)
        .map(|(i, _)| i as u8)
        .collect();
    let deck_at: std::collections::HashMap<(i32, i32), (i16, u8)> =
        il.decks.iter().map(|d| ((i32::from(d.x), i32::from(d.z)), (d.top, d.kind))).collect();
    let mut joints: std::collections::HashMap<(i32, i32), Vec<(i32, i32)>> = Default::default();
    for d in il.decks.iter().filter(|d| continuing.contains(&d.kind)) {
        let (x, z, top) = (i32::from(d.x), i32::from(d.z), i32::from(d.top));
        for (dx, dz) in [(1, 0), (-1, 0), (0, 1), (0, -1)] {
            let (nx, nz) = (x + dx, z + dz);
            if !(0..size_i).contains(&nx) || !(0..size_i).contains(&nz) {
                continue;
            }
            if let Some(&(t2, k2)) = deck_at.get(&(nx, nz)) {
                // A gangway landing on a pontoon (or another kind of deck) is connected.
                if !continuing.contains(&k2) && (top - i32::from(t2)).abs() <= 1 {
                    joints.entry((x, z)).or_default().push((nx, nz));
                }
                continue;
            }
            // Connected: land within a block of the deck (any land use), or a building
            // rising above it (bridges between warehouses lead into them).
            let bt = inp.building_top[nz as usize * size + nx as usize];
            if bt != i16::MIN && i32::from(bt) > top {
                joints.entry((x, z)).or_default().push((nx, nz));
                continue;
            }
            let j = g.idx(nx as usize, nz as usize);
            if g.water_top[j] != NO_WATER {
                continue;
            }
            let gy = i32::from(g.ground[j]);
            if (top - gy).abs() > 1 {
                continue;
            }
            joints.entry((x, z)).or_default().push((nx, nz));
            if !inp.surface_classes.contains(&g.class[j]) {
                continue;
            }
            let (a, b) = (name(x as usize, top, z as usize), name(nx as usize, gy, nz as usize));
            // Compared: deck cells over water (their surface is carried across from the
            // streets; over land it is ALKIS's own) against street surfaces, track beds aside.
            let over_land = g.water_top[g.idx(x as usize, z as usize)] == NO_WATER;
            if over_land || a == inp.ballast || !inp.surface_tops.contains(&b) {
                continue;
            }
            r.surface_joints += 1;
            // A lane boundary crossing the joint at an angle steps by a cell: the deck's
            // surface on the street within a cell of the joint is aligned.
            let near = |(px, pz): (i32, i32)| {
                (0..size_i).contains(&px)
                    && (0..size_i).contains(&pz)
                    && !deck_at.contains_key(&(px, pz))
                    && {
                        let q = g.idx(px as usize, pz as usize);
                        g.water_top[q] == NO_WATER
                            && name(px as usize, i32::from(g.ground[q]), pz as usize) == a
                    }
            };
            let stepped = (-1..=1).any(|ez| (-1..=1).any(|ex| near((nx + ex, nz + ez))));
            if a != b && !stepped {
                r.surface_mismatches += 1;
                let key = format!(
                    "{} -> {}",
                    a.trim_start_matches("minecraft:"),
                    b.trim_start_matches("minecraft:")
                );
                *r.mismatch_pairs.entry(key).or_default() += 1;
                if r.mismatch_samples.len() < 30 {
                    r.mismatch_samples.push([bx + x, top, bz + z]);
                }
            }
        }
    }
    // Cliffs within decks; one finding per 8 m square.
    let mut cliff_cells: std::collections::BTreeSet<(i32, i32)> = Default::default();
    for d in &il.decks {
        let (x, z) = (i32::from(d.x), i32::from(d.z));
        for n in [(x + 1, z), (x, z + 1)] {
            if let Some(&(t2, k2)) = deck_at.get(&n)
                && k2 == d.kind
                && (i32::from(t2) - i32::from(d.top)).abs() >= 2
            {
                r.deck_cliffs += 1;
                cliff_cells.insert((x.div_euclid(8), z.div_euclid(8)));
            }
        }
    }
    for (cx, cz) in cliff_cells {
        r.deck_findings.push(("cliff".into(), [bx + cx * 8 + 4, bz + cz * 8 + 4]));
    }
    // Components of bridge deck columns (4-neighborhood).
    let mut seen: std::collections::HashSet<(i32, i32)> = Default::default();
    let mut keys: Vec<&(i32, i32)> =
        deck_at.iter().filter(|(_, v)| continuing.contains(&v.1)).map(|(k, _)| k).collect();
    keys.sort();
    for &start in keys {
        if !seen.insert(start) {
            continue;
        }
        let mut comp = vec![start];
        let mut k = 0;
        while k < comp.len() {
            let (x, z) = comp[k];
            k += 1;
            for (dx, dz) in [(1, 0), (-1, 0), (0, 1), (0, -1)] {
                let n = (x + dx, z + dz);
                if deck_at.get(&n).is_some_and(|v| continuing.contains(&v.1)) && seen.insert(n) {
                    comp.push(n);
                }
            }
        }
        if comp.len() < 8 {
            continue;
        }
        // Decks cut by the tile edge are judged by the tile holding most of them.
        r.bridge_decks += 1;
        let js: Vec<(i32, i32)> = comp.iter().filter(|c| joints.contains_key(c)).copied().collect();
        let extent = {
            let (x0, x1) =
                (comp.iter().map(|c| c.0).min().unwrap(), comp.iter().map(|c| c.0).max().unwrap());
            let (z0, z1) =
                (comp.iter().map(|c| c.1).min().unwrap(), comp.iter().map(|c| c.1).max().unwrap());
            f64::from(x1 - x0).hypot(f64::from(z1 - z0))
        };
        let touches_edge =
            comp.iter().any(|c| c.0 == 0 || c.1 == 0 || c.0 == size_i - 1 || c.1 == size_i - 1);
        if js.is_empty() {
            if !touches_edge {
                r.isolated_decks += 1;
                r.deck_findings.push(("isolated".into(), [bx + comp[0].0, bz + comp[0].1]));
            }
            continue;
        }
        let spread = js
            .iter()
            .flat_map(|a| js.iter().map(move |b| f64::from(a.0 - b.0).hypot(f64::from(a.1 - b.1))))
            .fold(0.0f64, f64::max);
        if spread < 0.5 * extent && !touches_edge {
            r.one_sided_decks += 1;
            r.deck_findings.push(("one-sided".into(), [bx + js[0].0, bz + js[0].1]));
        }
    }
    let piers: std::collections::HashSet<(u16, u16)> =
        il.piers.iter().map(|p| (p.0, p.1)).collect();
    // Columns of floating decks: decks above them rest on the pontoon, not on water.
    let floating: std::collections::HashSet<(u16, u16)> = il
        .decks
        .iter()
        .filter(|d| inp.cfg.deck[d.kind as usize].floating.is_some())
        .map(|d| (d.x, d.z))
        .collect();
    for d in &il.decks {
        let kind = &inp.cfg.deck[d.kind as usize];
        let (x, z) = (d.x as usize, d.z as usize);
        let i = g.idx(x, z);
        if g.water_top[i] == NO_WATER {
            continue;
        }
        let (gy, wt) = (i32::from(g.ground[i]), i32::from(g.water_top[i]));
        if let Some(f) = kind.floating {
            r.floating_columns += 1;
            let top = i32::from(d.top);
            let ok = top == wt + f.freeboard
                && !is(x, top, z, "minecraft:air")
                && !is(x, top, z, "minecraft:water")
                && !is(x, wt, z, "minecraft:water");
            if !ok {
                r.floating_off_level += 1;
                sample(&mut r, x, top, z);
            }
            continue;
        }
        if piers.contains(&(d.x, d.z)) || floating.contains(&(d.x, d.z)) || building(x, z) {
            continue;
        }
        r.bridge_water_columns += 1;
        let water_ok = (gy + 1..=wt).all(|y| is(x, y, z, "minecraft:water"));
        let bottom = i32::from(d.bottom);
        let air = (wt + 1..bottom).filter(|&y| is(x, y, z, "minecraft:air")).count() as i32;
        let need = i32::from(d.clearance);
        if !water_ok || air < need || bottom - wt - 1 < need {
            r.bridge_blocked += 1;
            sample(&mut r, x, wt, z);
        }
    }
    r.empty_decks = il.stats.empty_decks.clone();
    r.pass = r.elevated_gaps == 0
        && r.empty_decks.is_empty()
        && r.elevated_steps == 0
        && r.ground_rail_steps == 0
        && r.bridge_blocked == 0
        && r.floating_off_level == 0;
    r
}
