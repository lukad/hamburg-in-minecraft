//! Infra stage: turns the prepared structures of a tile
//! (`data/prepared/infra/<rx>_<rz>.json`) into blocks.
//!
//! - **Decks** (bridges, elevated railways and roads, landing stages): one height surface
//!   per deck kind over all its polygons, from a low quantile of unobstructed bDOM,
//!   smoothed, and pinned to the streets where a deck meets them (see config/infra.toml
//!   `[heights]`). Bridges keep `min_clearance` air above water. Floating decks (landing
//!   stages) sit `freeboard` above the water with their hull in its top `draft` blocks.
//!   With `surface_from_landuse`, the road, sidewalk and plaza surfaces of the streets
//!   meeting a deck continue across it. Railings go on edges with a drop beside them (not
//!   where the deck lands on the ground) and are closed around diagonal corners.
//! - **Piers** reach from the ground (river bed) to the underside of the deck above.
//! - **Rails** follow the tracks: on decks (with ballast), across gaps between deck
//!   polygons at the interpolated height, and on the ground on a smoothed bed.
//!
//! Everything is computed on the tile's buffered grid from inputs that are identical for
//! both tiles at an edge, so structures crossing tile edges match.

mod crossing;
mod heights;
mod tracks;

use std::collections::{BTreeMap, HashMap, VecDeque};

use anyhow::{Context, Result};
use hhmc_core::config::{HeightMapping, Origin};
use hhmc_core::geom::rasterize_polygon;
use hhmc_core::ground::{GroundModel, NO_WATER};
use hhmc_core::infra::{InfraConfig, TileInfra};
use hhmc_core::landcfg::BlocksConfig;
use hhmc_core::raster::{Grid, NODATA};
use hhmc_core::{BlockState, TILE_SIZE};
use serde::{Deserialize, Serialize};

pub use tracks::{grid_path, rail_shape};

/// Bump when the infra logic changes.
pub const INFRA_VERSION: u32 = 52;
/// Holes in a deck's abutments up to this many cells are filled.
const MAX_HOLE: usize = 25;

/// A deck cell of the tile: (x, z) tile-local, deck kind index, top and bottom y.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct DeckColumn {
    pub x: u16,
    pub z: u16,
    pub kind: u8,
    pub top: i16,
    pub bottom: i16,
    /// Air blocks required between the water and the deck's underside here.
    pub clearance: i8,
}

/// One cell of a track's path: world (x, z), the surface y the rail stands on, and the
/// kind of deck carrying it (if any).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct RailCell {
    pub x: i32,
    pub z: i32,
    pub surface: i16,
    pub deck: Option<u8>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct InfraLayer {
    pub palette: Vec<BlockState>,
    /// (x, z, y0, y1) cleared to air before the blocks (track beds cut into the ground).
    pub clears: Vec<(u16, u16, i16, i16)>,
    /// (x, y, z, palette index), tile-local.
    pub blocks: Vec<(u16, i16, u16, u16)>,
    /// Drawn deck cells.
    pub decks: Vec<DeckColumn>,
    /// Abutment cells (see infra.toml `abutment_step_m`) with a deck, on the buffered grid:
    /// (world x, world z, deck top). The street is up there: trees stand on it.
    #[serde(default)]
    pub abutments: Vec<(i32, i32, i16)>,
    /// Deck edge cells where the deck lands (see `anchor_step_m`): (x, z, deck top, highest
    /// ground y of the land it lands on), tile-local.
    pub street_contacts: Vec<(u16, u16, i16, i16)>,
    /// (x, z, bottom, top) of pier columns, tile-local.
    pub piers: Vec<(u16, u16, i16, i16)>,
    /// Track paths through the tile (cells inside the tile, in path order).
    pub tracks: Vec<(String, Vec<RailCell>)>,
    /// Applied after the buildings: track corridors on decks with
    /// `pass_through_clearance`, their rails, and the air above them.
    pub override_clears: Vec<(u16, u16, i16, i16)>,
    pub override_blocks: Vec<(u16, i16, u16, u16)>,
    pub stats: InfraStats,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct InfraStats {
    pub decks: usize,
    pub deck_columns: usize,
    /// Water cells added between deck ends and the banks.
    pub bank_extension_cells: usize,
    /// Street land added where the deck's bDOM runs on over dipped DGM1 (abutments).
    pub abutment_cells: usize,
    /// Deck cells over land at street level (not drawn).
    pub street_level_cells: usize,
    /// Deck polygons with cells over water in this tile that produced no deck column
    /// there (coverage).
    #[serde(default)]
    pub empty_decks: Vec<String>,
    /// Deck cells raised to keep the clearance above water.
    pub raised_for_clearance: usize,
    /// Deck cells without any bDOM value within reach (not drawn).
    pub no_height: usize,
    pub railing_blocks: usize,
    pub pier_columns: usize,
    pub rail_blocks: usize,
    /// Track cells on decks added across gaps between deck polygons.
    pub gap_deck_cells: usize,
    /// Track bed cells cut into or filled onto the ground.
    pub bed_cut: usize,
    pub bed_fill: usize,
}

impl InfraLayer {
    /// Deck tops of the abutment cells by world column.
    pub fn abutment_tops(&self) -> HashMap<(i32, i32), i32> {
        self.abutments.iter().map(|&(x, z, y)| ((x, z), i32::from(y))).collect()
    }

    /// Top of the floating deck per tile column (`z * 512 + x`), `i16::MIN` where none:
    /// buildings on landing stages stand on it.
    pub fn floating_tops(&self, cfg: &InfraConfig) -> Vec<i16> {
        let size = TILE_SIZE as usize;
        let mut v = vec![i16::MIN; size * size];
        for d in &self.decks {
            if cfg.deck[d.kind as usize].floating.is_some() {
                let k = d.z as usize * size + d.x as usize;
                v[k] = v[k].max(d.top);
            }
        }
        v
    }
}

pub struct InfraInputs<'a> {
    pub structures: &'a TileInfra,
    pub cfg: &'a InfraConfig,
    pub blocks: &'a BlocksConfig,
    pub origin: &'a Origin,
    pub height: &'a HeightMapping,
    pub ground: &'a GroundModel,
    /// bDOM on the tile's read grid.
    pub bdom: &'a Grid<f32>,
    /// DGM1 (bare earth) on the tile's read grid; without it, the ground model's heights.
    pub dgm: Option<&'a Grid<f32>>,
    /// ALKIS land-use classes as published (before prep turned the road under bridges
    /// into water), on the tile's read grid.
    pub alkis_class: Option<&'a Grid<u8>>,
}

/// Block priorities: later kinds win in a cell.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Prio {
    Bed,
    Deck,
    Pier,
    Railing,
    Rail,
}

/// The tile's buffered grid (the ground model's).
pub(crate) struct Cells {
    pub w: usize,
    pub h: usize,
    pub border: i32,
}

impl Cells {
    pub fn local(&self, i: usize) -> (i32, i32) {
        ((i % self.w) as i32 - self.border, (i / self.w) as i32 - self.border)
    }
    pub fn in_tile(&self, i: usize) -> bool {
        let (x, z) = self.local(i);
        (0..TILE_SIZE).contains(&x) && (0..TILE_SIZE).contains(&z)
    }
    pub fn col_row(&self, i: usize) -> (i64, i64) {
        ((i % self.w) as i64, (i / self.w) as i64)
    }
    pub fn at(&self, c: i64, r: i64) -> Option<usize> {
        (c >= 0 && r >= 0 && (c as usize) < self.w && (r as usize) < self.h)
            .then(|| r as usize * self.w + c as usize)
    }
    pub fn neighbors(&self, i: usize) -> impl Iterator<Item = usize> + '_ {
        let (c, r) = self.col_row(i);
        [(1i64, 0i64), (-1, 0), (0, 1), (0, -1)]
            .into_iter()
            .filter_map(move |(dc, dr)| self.at(c + dc, r + dr))
    }
}

/// A deck cell on the buffered grid (one per kind and cell).
#[derive(Clone, Copy, Debug)]
pub(crate) struct DeckCell {
    pub kind: usize,
    pub cell: usize,
    pub top: i32,
    pub bottom: i32,
    pub clearance: i32,
}

pub fn infra(inp: &InfraInputs) -> Result<InfraLayer> {
    let (cfg, g) = (inp.cfg, inp.ground);
    let cells = Cells { w: g.w, h: g.h, border: ((g.w - TILE_SIZE as usize) / 2) as i32 };
    let mut stats = InfraStats::default();
    let parse = |s: &str| BlockState::parse(s).map_err(anyhow::Error::msg);
    let mut palette: Vec<BlockState> = Vec::new();
    let mut intern = |s: BlockState| -> u16 {
        if let Some(i) = palette.iter().position(|p| *p == s) {
            return i as u16;
        }
        palette.push(s);
        (palette.len() - 1) as u16
    };
    let ground = |i: usize| i32::from(g.ground[i]);
    let water = |i: usize| (g.water_top[i] != NO_WATER).then(|| i32::from(g.water_top[i]));
    let bdom_at = |i: usize| -> Option<f32> {
        let (col, row) = (i % g.w, i / g.w);
        let e = g.min_e + col as f64 + 0.5;
        let n = g.max_n - row as f64 - 0.5;
        inp.bdom.at(e, n).filter(|&v| v != NODATA)
    };
    let dgm_at = |i: usize| -> f32 {
        let (col, row) = (i % g.w, i / g.w);
        inp.dgm
            .and_then(|d| d.at(g.min_e + col as f64 + 0.5, g.max_n - row as f64 - 0.5))
            .filter(|&v| v != NODATA)
            .unwrap_or(f32::from(g.ground[i]))
    };
    // The street (bDOM `b`) runs on above the bare earth of cell `n` (see abutments).
    let raised = |n: usize, b: f32| {
        f64::from(b - dgm_at(n)) >= cfg.heights.abutment_min_rise_m
            && inp.height.to_y(f64::from(b)).y > ground(n)
    };
    let structures = inp.structures;
    let nkinds = cfg.deck.len();
    let alkis = |i: usize| -> Option<u8> {
        let a = inp.alkis_class?;
        let (col, row) = (i % g.w, i / g.w);
        a.at(g.min_e + col as f64 + 0.5, g.max_n - row as f64 - 0.5)
    };
    let water_class = inp.blocks.classes.iter().position(|c| c.water).map(|i| i as u8);
    let under_bridge =
        |i: usize| water(i).is_some() && alkis(i).is_some_and(|c| Some(c) != water_class);

    // Rasterize decks: per polygon, and the union per kind.
    let mut deck_cells: Vec<Vec<usize>> = Vec::with_capacity(structures.decks.len());
    let mut kind_mask: Vec<Vec<bool>> = vec![vec![false; g.w * g.h]; nkinds];
    for d in &structures.decks {
        let mut v = Vec::new();
        for rings in &d.polygons {
            rasterize_polygon(rings, g.min_e, g.max_n, g.w, g.h, |c, r| v.push(r * g.w + c));
        }
        v.sort_unstable();
        v.dedup();
        for &i in &v {
            kind_mask[d.kind][i] = true;
        }
        if v.iter().any(|&i| cells.in_tile(i)) {
            stats.decks += 1;
        }
        deck_cells.push(v);
    }
    // Street surfaces: anchors, street contacts and surfaces continued across decks.
    let surface_ids: Vec<u8> = cfg
        .surfaces
        .classes
        .iter()
        .map(|n| {
            inp.blocks
                .classes
                .iter()
                .position(|c| &c.id == n)
                .map(|i| i as u8)
                .with_context(|| format!("infra.toml: unknown surface class {n:?}"))
        })
        .collect::<Result<_>>()?;
    // Abutments (see infra.toml `abutment_step_m`): street land the deck's bDOM runs on
    // over while DGM1 dips below it.
    let mut occluded = vec![false; g.w * g.h];
    for rings in &structures.occluders {
        rasterize_polygon(rings, g.min_e, g.max_n, g.w, g.h, |c, r| occluded[r * g.w + c] = true);
    }
    // Street land, including the road under a bridge (water in the ground model).
    let street = |n: usize| {
        let class = if water(n).is_none() {
            Some(g.class[n])
        } else {
            alkis(n).filter(|_| under_bridge(n))
        };
        class.is_some_and(|c| surface_ids.contains(&c))
    };
    let mut abutment = vec![false; g.w * g.h];
    for (k, kind) in cfg.deck.iter().enumerate() {
        if !kind.surface_from_landuse || kind.floating.is_some() {
            continue;
        }
        let free = |i: usize| (!occluded[i]).then(|| bdom_at(i)).flatten();
        let mut depth: HashMap<usize, (u32, f32)> = HashMap::new();
        let mut q: VecDeque<usize> = VecDeque::new();
        for i in (0..g.w * g.h).filter(|&i| kind_mask[k][i]) {
            if let Some(b) = free(i).filter(|_| !(0..nkinds).any(|o| o != k && kind_mask[o][i])) {
                depth.insert(i, (0, b));
                q.push_back(i);
            }
        }
        let max = cfg.heights.abutment_max_m.round() as u32;
        let mut added = Vec::new();
        while let Some(i) = q.pop_front() {
            let (d, b) = depth[&i];
            if d >= max {
                continue;
            }
            for n in cells.neighbors(i) {
                if depth.contains_key(&n) || kind_mask.iter().any(|m| m[n]) || !street(n) {
                    continue;
                }
                let Some(bn) = free(n) else { continue };
                // Something standing on the street hides it: the cell joins (without
                // spreading further) where its bare earth lies below the abutment.
                if f64::from(bn - b) > cfg.heights.abutment_step_m {
                    if raised(n, b) {
                        depth.insert(n, (max, b));
                        added.push(n);
                    }
                    continue;
                }
                if f64::from((bn - b).abs()) > cfg.heights.abutment_step_m || !raised(n, bn) {
                    continue;
                }
                depth.insert(n, (d + 1, bn));
                added.push(n);
                q.push_back(n);
            }
        }
        for &c in &added {
            kind_mask[k][c] = true;
        }
        // Holes: where bDOM does not continue the deck (a tree, its pit, a vehicle) or the
        // rise is within the noise, cells enclosed by the deck on three sides whose ground
        // lies below it are part of it.
        loop {
            let mut more = Vec::new();
            for &i in &added {
                for n in cells.neighbors(i) {
                    if kind_mask.iter().any(|m| m[n]) || !street(n) || more.contains(&n) {
                        continue;
                    }
                    let around: Vec<f32> = cells
                        .neighbors(n)
                        .filter(|&m| kind_mask[k][m])
                        .filter_map(|m| depth.get(&m).map(|v| v.1))
                        .collect();
                    if cells.neighbors(n).filter(|&m| kind_mask[k][m]).count() < 3
                        || around.is_empty()
                    {
                        continue;
                    }
                    let b = around.iter().sum::<f32>() / around.len() as f32;
                    if !occluded[n] && inp.height.to_y(f64::from(b)).y > ground(n) {
                        depth.insert(n, (max, b));
                        more.push(n);
                    }
                }
            }
            if more.is_empty() {
                break;
            }
            for &c in &more {
                kind_mask[k][c] = true;
            }
            added.extend(more);
        }
        // Larger holes (up to `MAX_HOLE` cells) the deck encloses on all sides.
        let mut tried: std::collections::HashSet<usize> = Default::default();
        let starts: Vec<usize> =
            added.iter().flat_map(|&c| cells.neighbors(c).collect::<Vec<_>>()).collect();
        for s0 in starts {
            if kind_mask.iter().any(|m| m[s0]) || !street(s0) || occluded[s0] || !tried.insert(s0) {
                continue;
            }
            let mut comp = vec![s0];
            let mut enclosed = true;
            let mut q = 0;
            while q < comp.len() && enclosed {
                let c = comp[q];
                q += 1;
                for n in cells.neighbors(c) {
                    if kind_mask[k][n] || comp.contains(&n) {
                        continue;
                    }
                    if kind_mask.iter().any(|m| m[n]) || !street(n) || occluded[n] {
                        enclosed = false;
                        break;
                    }
                    comp.push(n);
                    tried.insert(n);
                    if comp.len() > MAX_HOLE {
                        enclosed = false;
                        break;
                    }
                }
            }
            if !enclosed {
                continue;
            }
            let around: Vec<f32> = comp
                .iter()
                .flat_map(|&c| cells.neighbors(c).collect::<Vec<_>>())
                .filter_map(|n| depth.get(&n).map(|v| v.1))
                .collect();
            if around.is_empty() {
                continue;
            }
            let b = around.iter().sum::<f32>() / around.len() as f32;
            if comp.iter().all(|&c| inp.height.to_y(f64::from(b)).y > ground(c)) {
                for &c in &comp {
                    depth.insert(c, (max, b));
                    kind_mask[k][c] = true;
                }
                added.extend(comp);
            }
        }
        // An abutment joins a deck to a street at its height: patches that touch no bare
        // land at their height (a canopy or a platform beside a deck between buildings) are
        // not one.
        let step = cfg.heights.anchor_step_m as f32;
        let in_added: std::collections::HashSet<usize> = added.iter().copied().collect();
        let mut seen: std::collections::HashSet<usize> = Default::default();
        let mut kept = Vec::new();
        for &s0 in &added {
            if !seen.insert(s0) {
                continue;
            }
            let mut comp = vec![s0];
            let mut q = 0;
            while q < comp.len() {
                let c = comp[q];
                q += 1;
                for n in cells.neighbors(c) {
                    if in_added.contains(&n) && seen.insert(n) {
                        comp.push(n);
                    }
                }
            }
            let joins = comp.iter().any(|&c| {
                let b = depth[&c].1;
                cells.neighbors(c).any(|n| {
                    !in_added.contains(&n)
                        && !kind_mask.iter().any(|m| m[n])
                        && water(n).is_none()
                        && !occluded[n]
                        && bdom_at(n).is_some_and(|bn| {
                            (bn - dgm_at(n)).abs() <= step && (bn - b).abs() <= step
                        })
                })
            });
            if joins {
                kept.extend(comp);
            } else {
                for c in comp {
                    kind_mask[k][c] = false;
                }
            }
        }
        let added = kept;
        stats.abutment_cells += added.iter().filter(|&&c| cells.in_tile(c)).count();
        for c in added {
            abutment[c] = true;
        }
    }
    // Crossings: each deck's direction from land to land; ends extended over short water
    // gaps to the banks (ALKIS bridge polygons stop a little short in places).
    let mut crossing_dir: Vec<HashMap<usize, (f64, f64)>> = vec![HashMap::new(); nkinds];
    for (k, kind) in cfg.deck.iter().enumerate() {
        if !kind.surface_from_landuse {
            continue;
        }
        let decks_now: Vec<bool> = (0..g.w * g.h).map(|i| kind_mask.iter().any(|m| m[i])).collect();
        let open_water = |i: usize| water(i).is_some() && !decks_now[i] && !under_bridge(i);
        let tol = cfg.heights.anchor_tolerance_m;
        let lands = |deck: usize, land: usize| {
            bdom_at(deck).is_none_or(|h| (f64::from(h) - f64::from(ground(land))).abs() <= tol)
        };

        let other_deck = |i: usize| decks_now[i] && !kind_mask[k][i];
        let cr = crossing::crossings(&crossing::CrossingInputs {
            cells: &cells,
            mask: &kind_mask[k],
            open_water: &open_water,
            under_bridge: &under_bridge,
            max_gap_m: cfg.heights.bank_gap_m,
            lands: &lands,
            other_deck: &other_deck,
        });
        for &c in &cr.extension {
            kind_mask[k][c] = true;
        }
        if cr.extension.iter().any(|&c| cells.in_tile(c)) {
            stats.bank_extension_cells +=
                cr.extension.iter().filter(|&&c| cells.in_tile(c)).count();
        }
        crossing_dir[k] = cr.dir;
    }
    let any_deck = |i: usize| kind_mask.iter().any(|m| m[i]);

    // Floating decks first: other decks resting on them land on their top.
    let mut dcells: Vec<DeckCell> = Vec::new();
    let mut floating_top: HashMap<usize, i32> = HashMap::new();
    for (k, kind) in cfg.deck.iter().enumerate() {
        let Some(f) = kind.floating else { continue };
        for i in (0..g.w * g.h).filter(|&i| kind_mask[k][i]) {
            if let Some(w) = water(i) {
                let top = w + f.freeboard;
                floating_top.insert(i, top);
                let bottom = top - f.freeboard - f.draft + 1;
                dcells.push(DeckCell { kind: k, cell: i, top, bottom, clearance: 0 });
            }
        }
    }
    // Polygons resting on a floating deck (gangways): no clearance over open water.
    let mut gangway = vec![false; g.w * g.h];
    for (di, d) in structures.decks.iter().enumerate() {
        if cfg.deck[d.kind].floating.is_none()
            && deck_cells[di].iter().any(|i| floating_top.contains_key(i))
        {
            for &i in &deck_cells[di] {
                gangway[i] = true;
            }
        }
    }

    // Fixed decks: one height surface per kind.
    let mut street_contacts: Vec<(u16, u16, i16, i16)> = Vec::new();
    for (k, kind) in cfg.deck.iter().enumerate() {
        if kind.floating.is_some() {
            continue;
        }
        let union: Vec<usize> = (0..g.w * g.h).filter(|&i| kind_mask[k][i]).collect();
        if union.is_empty() {
            continue;
        }
        let unobstructed =
            |i: usize| !occluded[i] && !(0..nkinds).any(|o| o != k && kind_mask[o][i]);
        // A clear deck sample: not beside another deck kind (bDOM there may see that deck
        // below a deck narrower than its polygon) and clear of the ground or water under it
        // (lower bDOM is the ground or water beside the deck, or seen through it).
        let clear = |i: usize, v: f32| {
            let (c, r) = cells.col_row(i);
            let apart = (-1..=1).all(|dr| {
                (-1..=1).all(|dc| {
                    cells
                        .at(c + dc, r + dr)
                        .is_none_or(|n| !(0..nkinds).any(|o| o != k && kind_mask[o][n]))
                })
            });
            apart
                && f64::from(v)
                    > f64::from(water(i).unwrap_or(ground(i)).max(ground(i)))
                        + cfg.heights.min_rise_m
        };
        let vals: HashMap<usize, (f32, bool, bool)> = union
            .iter()
            .filter_map(|&i| bdom_at(i).map(|v| (i, (v, unobstructed(i), clear(i, v)))))
            .collect();
        // Land a deck lands on (anchors): bare (its bDOM on DGM1: no dip under a hidden
        // abutment, nothing standing on it) and continuing the deck's measured surface (not
        // a bank, a road below or a street beside a viaduct).
        let step = cfg.heights.anchor_step_m as f32;
        let landing = |i: usize, n: usize| {
            let Some(bn) = (!occluded[n]).then(|| bdom_at(n)).flatten() else { return false };
            let edge = vals.get(&i).filter(|v| v.1).map(|v| v.0);
            (bn - dgm_at(n)).abs() <= step && edge.is_none_or(|e| (e - bn).abs() <= step)
        };
        let streets = |i: usize| -> Vec<i32> {
            cells
                .neighbors(i)
                .filter(|&n| !any_deck(n) && water(n).is_none() && landing(i, n))
                .map(ground)
                .collect()
        };
        let heights = heights::surface(&heights::SurfaceInputs {
            union: &union,
            vals: &vals,
            cells: &cells,
            cfg: &cfg.heights,
            streets: &streets,
        });
        for (&i, h) in union.iter().zip(heights) {
            let Some(h) = h else {
                if cells.in_tile(i) {
                    stats.no_height += 1;
                }
                continue;
            };
            let mut top = inp.height.to_y(f64::from(h)).y;
            let clearance = if gangway[i] { 0 } else { kind.min_clearance };
            let bottom = if let Some(&ft) = floating_top.get(&i) {
                // Resting on a floating deck: down to its top at most.
                top = top.max(ft);
                if top == ft {
                    continue;
                }
                (top - kind.thickness + 1).max(ft + 1)
            } else if let Some(w) = water(i) {
                let min_top = w + clearance + kind.thickness;
                if top < min_top {
                    top = min_top;
                    if cells.in_tile(i) {
                        stats.raised_for_clearance += 1;
                    }
                }
                top - kind.thickness + 1
            } else {
                if top <= ground(i) + i32::from(!abutment[i]) {
                    // At street level: the ground is the surface (on an abutment, where
                    // the deck is a block above the dipped ground, the deck is).
                    if cells.in_tile(i) {
                        stats.street_level_cells += 1;
                    }
                    continue;
                }
                top - kind.thickness + 1
            };
            dcells.push(DeckCell { kind: k, cell: i, top, bottom, clearance });
            if cells.in_tile(i)
                && let Some(gy) = streets(i).into_iter().max()
            {
                let (x, z) = cells.local(i);
                street_contacts.push((x as u16, z as u16, top as i16, gy as i16));
            }
        }
    }

    // Tracks: paths, gap decks between deck polygons, ground beds.
    let gap_kind = cfg.deck.iter().position(|d| d.kind == cfg.rails.gap_deck).expect("checked");
    let track_bdom =
        |i: usize| (!occluded[i] && !kind_mask.iter().any(|m| m[i])).then(|| bdom_at(i)).flatten();
    let viaduct_land: Vec<u8> = cfg
        .rails
        .viaduct_classes
        .iter()
        .map(|n| {
            inp.blocks
                .classes
                .iter()
                .position(|c| &c.id == n)
                .map(|i| i as u8)
                .with_context(|| format!("infra.toml: unknown surface class {n:?}"))
        })
        .collect::<Result<_>>()?;
    let tr = tracks::plan(&tracks::TrackInputs {
        tracks: &structures.tracks,
        origin: inp.origin,
        ground: g,
        cells: &cells,
        dcells: &dcells,
        cfg,
        gap_kind,
        bdom: &track_bdom,
        viaduct_land: &viaduct_land,
    });
    stats.gap_deck_cells = tr.gap_decks.iter().filter(|d| cells.in_tile(d.cell)).count();
    let gap_cells: std::collections::HashSet<usize> = tr.gap_decks.iter().map(|d| d.cell).collect();
    dcells.extend(tr.gap_decks.iter().copied());

    // Per cell: its deck cells.
    let mut at: HashMap<usize, Vec<usize>> = HashMap::new();
    for (k, dc) in dcells.iter().enumerate() {
        at.entry(dc.cell).or_default().push(k);
    }

    // Surfaces carried across decks from the streets meeting them.
    let mut surface_of: HashMap<(usize, usize), u8> = HashMap::new();
    for (k, kind) in cfg.deck.iter().enumerate() {
        if !kind.surface_from_landuse {
            continue;
        }
        let open_water = |i: usize| water(i).is_some() && !any_deck(i) && !under_bridge(i);
        let tol = cfg.heights.anchor_tolerance_m;
        let lands = |deck: usize, land: usize| {
            bdom_at(deck).is_none_or(|h| (f64::from(h) - f64::from(ground(land))).abs() <= tol)
        };

        let other_deck = |i: usize| any_deck(i) && !kind_mask[k][i];
        let ci = crossing::CrossingInputs {
            cells: &cells,
            mask: &kind_mask[k],
            open_water: &open_water,
            under_bridge: &under_bridge,
            max_gap_m: cfg.heights.bank_gap_m,
            lands: &lands,
            other_deck: &other_deck,
        };
        let street_class = |c: usize| surface_ids.contains(&g.class[c]).then_some(g.class[c]);
        // Lanes and sidewalks lined up with the streets at the crossing's ends.
        let mut q: VecDeque<usize> = VecDeque::new();
        for i in (0..g.w * g.h).filter(|&i| kind_mask[k][i]) {
            // The deck's own land use: the ground model's on land, ALKIS's on the road
            // under a bridge.
            let own = if water(i).is_none() {
                Some(g.class[i])
            } else {
                alkis(i).filter(|_| under_bridge(i))
            }
            .filter(|c| surface_ids.contains(c));
            let lined = || {
                crossing_dir[k]
                    .get(&i)
                    .and_then(|&d| crossing::lined_up_class(&ci, i, d, &street_class))
            };
            if let Some(c) = own.or_else(lined) {
                surface_of.insert((k, i), c);
                q.push_back(i);
            }
        }
        // Cells no street lines up with: the nearest surface on the deck.
        while let Some(i) = q.pop_front() {
            let c = surface_of[&(k, i)];
            for n in cells.neighbors(i) {
                if kind_mask[k][n] && !surface_of.contains_key(&(k, n)) {
                    surface_of.insert((k, n), c);
                    q.push_back(n);
                }
            }
        }
    }
    let class_tops: Vec<u16> =
        inp.blocks.classes.iter().map(|c| parse(&c.top).map(&mut intern)).collect::<Result<_>>()?;
    let kind_ids: Vec<(u16, u16, BlockState)> = cfg
        .deck
        .iter()
        .map(|k| Ok((intern(parse(&k.top)?), intern(parse(&k.structure)?), parse(&k.railing)?)))
        .collect::<Result<_>>()?;
    let ballast = intern(parse(&cfg.rails.ballast)?);

    let mut vox: BTreeMap<(i32, i32, i32), (Prio, u16)> = BTreeMap::new();
    let mut put = |x: i32, y: i32, z: i32, p: Prio, b: u16| {
        if !(0..TILE_SIZE).contains(&x) || !(0..TILE_SIZE).contains(&z) {
            return;
        }
        match vox.get(&(x, y, z)) {
            Some(&(q, _)) if q > p => {}
            _ => {
                vox.insert((x, y, z), (p, b));
            }
        }
    };

    // Deck blocks.
    let mut decks_out: Vec<DeckColumn> = Vec::new();
    for dc in &dcells {
        if !cells.in_tile(dc.cell) {
            continue;
        }
        let (x, z) = cells.local(dc.cell);
        let (top_id, structure_id, _) = &kind_ids[dc.kind];
        let top_block = if tr.corridor.contains(&dc.cell) || gap_cells.contains(&dc.cell) {
            ballast
        } else {
            surface_of.get(&(dc.kind, dc.cell)).map_or(*top_id, |&c| class_tops[c as usize])
        };
        let gy = ground(dc.cell);
        for y in dc.bottom.max(gy)..=dc.top {
            put(x, y, z, Prio::Deck, if y == dc.top { top_block } else { *structure_id });
        }
        decks_out.push(DeckColumn {
            x: x as u16,
            z: z as u16,
            kind: dc.kind as u8,
            top: dc.top as i16,
            bottom: dc.bottom as i16,
            clearance: dc.clearance as i8,
        });
        stats.deck_columns += 1;
    }

    // Railings: deck edges with a drop beside them, closed around diagonal corners.
    let mut railing: BTreeMap<(i32, i32, i32), usize> = BTreeMap::new();
    let surface_y = |n: usize| -> i32 {
        let mut s = water(n).unwrap_or(i32::MIN).max(ground(n));
        if let Some(ks) = at.get(&n) {
            s = s.max(ks.iter().map(|&k| dcells[k].top).max().unwrap_or(s));
        }
        s
    };
    for dc in &dcells {
        let kind = &cfg.deck[dc.kind];
        let edge = cells.neighbors(dc.cell).any(|n| {
            let joined =
                at.get(&n).is_some_and(|ks| ks.iter().any(|&k| dcells[k].top >= dc.top - 1));
            let drop = dc.top - surface_y(n);
            // The deck's ends land on the ground within anchor reach: no railing there.
            let landing = water(n).is_none()
                && !at.contains_key(&n)
                && f64::from(drop) <= cfg.heights.anchor_tolerance_m;
            !joined && !landing && drop >= kind.railing_min_drop
        });
        if edge && !tr.corridor.contains(&dc.cell) {
            let (x, z) = cells.local(dc.cell);
            railing.insert((x, dc.top + 1, z), dc.kind);
        }
    }
    // Diagonal neighbors only touch at an edge: add the inner corner cell (a deck cell
    // with the same top) so the railing is continuous.
    let deck_top_at = |x: i32, z: i32| -> Option<i32> {
        let i = cells.at(i64::from(x + cells.border), i64::from(z + cells.border))?;
        at.get(&i).and_then(|ks| ks.iter().map(|&k| dcells[k].top).max())
    };
    let mut corners: Vec<((i32, i32, i32), usize)> = Vec::new();
    for (&(x, y, z), &k) in &railing {
        for dz in [1, -1] {
            if !railing.contains_key(&(x + 1, y, z + dz)) {
                continue;
            }
            let (a, b) = ((x + 1, z), (x, z + dz));
            if railing.contains_key(&(a.0, y, a.1)) || railing.contains_key(&(b.0, y, b.1)) {
                continue;
            }
            if let Some(c) = [a, b].into_iter().find(|&(cx, cz)| deck_top_at(cx, cz) == Some(y - 1))
            {
                corners.push(((c.0, y, c.1), k));
            }
        }
    }
    railing.extend(corners);
    open_walkways(&cells, &dcells, &at, &mut railing, &|n| water(n).is_none().then(|| ground(n)));
    for (&(x, y, z), &k) in &railing {
        let mut s = kind_ids[k].2.clone();
        if s.name.ends_with("iron_bars") || s.name.ends_with("_pane") || s.name.ends_with("fence") {
            for (name, dx, dz) in
                [("north", 0, -1), ("south", 0, 1), ("east", 1, 0), ("west", -1, 0)]
            {
                let v = if railing.contains_key(&(x + dx, y, z + dz)) { "true" } else { "false" };
                s = s.with(name, v);
            }
            s = s.with("waterlogged", "false");
        }
        if (0..TILE_SIZE).contains(&x) && (0..TILE_SIZE).contains(&z) {
            stats.railing_blocks += 1;
        }
        put(x, y, z, Prio::Railing, intern(s));
    }

    // Piers: up to the underside of the deck above (within one cell).
    let pier_block = intern(parse(&cfg.piers.block)?);
    let support_block = intern(parse(&cfg.piers.support_block)?);
    let mut piers_out = Vec::new();
    for p in &structures.piers {
        let mut cs = Vec::new();
        for rings in &p.polygons {
            rasterize_polygon(rings, g.min_e, g.max_n, g.w, g.h, |c, r| cs.push(r * g.w + c));
        }
        if cs.is_empty() {
            // Narrower than a cell: its center.
            let r = &p.polygons[0][0];
            let n = r.len().max(1) as f64;
            let (e, nn) =
                (r.iter().map(|q| q.0).sum::<f64>() / n, r.iter().map(|q| q.1).sum::<f64>() / n);
            if let Some(i) = cells.at((e - g.min_e).floor() as i64, (g.max_n - nn).floor() as i64) {
                cs.push(i);
            }
        }
        for &i in &cs {
            if !cells.in_tile(i) {
                continue;
            }
            let gy = ground(i);
            let (c, r) = cells.col_row(i);
            let mut under: Option<i32> = None;
            for (dc, dr) in [(0, 0), (1, 0), (-1, 0), (0, 1), (0, -1)] {
                let Some(n) = cells.at(c + dc, r + dr) else { continue };
                for &k in at.get(&n).map_or(&[][..], |v| v) {
                    let b = dcells[k].bottom;
                    if b > gy + 1 && cfg.deck[dcells[k].kind].floating.is_none() {
                        under = Some(under.map_or(b, |u: i32| u.min(b)));
                    }
                }
                if under.is_some() {
                    break;
                }
            }
            let Some(b) = under else { continue };
            let top = b - 1;
            let (x, z) = cells.local(i);
            let blk = if p.generated { support_block } else { pier_block };
            for y in gy + 1..=top {
                put(x, y, z, Prio::Pier, blk);
            }
            piers_out.push((x as u16, z as u16, (gy + 1) as i16, top as i16));
            stats.pier_columns += 1;
        }
    }

    // Track beds and rails.
    let mut clears: Vec<(u16, u16, i16, i16)> = Vec::new();
    for &(i, bed) in &tr.beds {
        if !cells.in_tile(i) {
            continue;
        }
        let (x, z) = cells.local(i);
        let gy = ground(i);
        if gy > bed {
            clears.push((x as u16, z as u16, (bed + 1) as i16, gy as i16));
            stats.bed_cut += 1;
        } else if gy < bed {
            stats.bed_fill += 1;
        }
        for y in gy.min(bed)..=bed {
            put(x, y, z, Prio::Bed, ballast);
        }
    }
    let rail_state = parse(&cfg.rails.block)?;
    for &(i, y, shape) in &tr.rails {
        if !cells.in_tile(i) {
            continue;
        }
        let (x, z) = cells.local(i);
        put(x, y, z, Prio::Rail, intern(rail_state.clone().with("shape", shape)));
        stats.rail_blocks += 1;
    }

    // Pass-through: track corridors on decks of kinds that ask for it.
    let mut through: HashMap<(i32, i32), (i32, i32)> = HashMap::new();
    for dc in &dcells {
        let c = cfg.deck[dc.kind].pass_through_clearance;
        if c > 0 && cells.in_tile(dc.cell) && tr.corridor.contains(&dc.cell) {
            let (x, z) = cells.local(dc.cell);
            // Never below the ground block: it is not ours to clear, and nothing would
            // put it back (station halls whose deck starts at the ground left holes).
            let lo = dc.bottom.max(ground(dc.cell) + 1);
            let e = through.entry((x, z)).or_insert((lo, dc.top + c));
            *e = (e.0.min(lo), e.1.max(dc.top + c));
        }
    }
    let mut override_clears: Vec<(u16, u16, i16, i16)> = Vec::new();
    let mut override_blocks: Vec<(u16, i16, u16, u16)> = Vec::new();
    let mut keys: Vec<&(i32, i32)> = through.keys().collect();
    keys.sort();
    for &(x, z) in keys {
        let (lo, hi) = through[&(x, z)];
        override_clears.push((x as u16, z as u16, lo as i16, hi as i16));
        for ((_, y, _), &(_, b)) in
            vox.range((x, lo, z)..=(x, hi, z)).filter(|((vx, _, vz), _)| (*vx, *vz) == (x, z))
        {
            override_blocks.push((x as u16, *y as i16, z as u16, b));
        }
    }
    let blocks: Vec<(u16, i16, u16, u16)> =
        vox.into_iter().map(|((x, y, z), (_, b))| (x as u16, y as i16, z as u16, b)).collect();
    // Coverage: every deck over water draws something.
    let drawn: std::collections::HashSet<usize> = dcells.iter().map(|d| d.cell).collect();
    for (d, cs) in structures.decks.iter().zip(&deck_cells) {
        let over_water: Vec<usize> =
            cs.iter().copied().filter(|&i| cells.in_tile(i) && water(i).is_some()).collect();
        if !over_water.is_empty() && !over_water.iter().any(|i| drawn.contains(i)) {
            stats.empty_decks.push(d.id.clone());
        }
    }
    let (bx, bz) = g.key.min_block();
    // Debugging: `HHMC_DEBUG_CELLS=x0,x1,z0,z1,file` writes one line per world cell of the
    // window: x z flags deck_top ground dgm bdom ground_class alkis_class.
    if let Ok(dbg) = std::env::var("HHMC_DEBUG_CELLS") {
        let p: Vec<&str> = dbg.split(',').collect();
        let n = |k: usize| p[k].parse::<i32>().expect("HHMC_DEBUG_CELLS=x0,x1,z0,z1,file");
        let top: HashMap<usize, (i32, usize)> =
            dcells.iter().map(|d| (d.cell, (d.top, d.kind))).collect();
        let mut out = String::new();
        for z in n(2)..=n(3) {
            for x in n(0)..=n(1) {
                let Some(i) = g.idx_buffered(x - bx, z - bz) else { continue };
                let poly = deck_cells.iter().any(|d| d.binary_search(&i).is_ok());
                let kind = (0..nkinds).find(|&k| kind_mask[k][i]);
                let flags = format!(
                    "{}{}{}{}{}{}",
                    kind.map_or('-', |k| (b'0' + k as u8) as char),
                    if poly { 'P' } else { '-' },
                    if abutment[i] { 'A' } else { '-' },
                    if under_bridge(i) {
                        'u'
                    } else if water(i).is_some() {
                        'w'
                    } else {
                        '-'
                    },
                    if occluded[i] { 'o' } else { '-' },
                    if gap_cells.contains(&i) { 'g' } else { '-' },
                );
                out += &format!(
                    "{x} {z} {flags} {} {} {:.2} {} {} {}\n",
                    top.get(&i).map_or("-".into(), |t| t.0.to_string()),
                    ground(i),
                    dgm_at(i),
                    bdom_at(i).map_or("-".into(), |b| format!("{b:.2}")),
                    g.class[i],
                    alkis(i).map_or("-".into(), |c| c.to_string()),
                );
            }
        }
        std::fs::write(p[4], out).ok();
    }
    let mut abutments: Vec<(i32, i32, i16)> = dcells
        .iter()
        .filter(|dc| abutment[dc.cell])
        .map(|dc| {
            let (x, z) = cells.local(dc.cell);
            (x + bx, z + bz, dc.top as i16)
        })
        .collect();
    abutments.sort_unstable();
    abutments.dedup_by_key(|a| (a.0, a.1));
    Ok(InfraLayer {
        palette,
        clears,
        blocks,
        decks: decks_out,
        abutments,
        street_contacts,
        piers: piers_out,
        tracks: tr.paths,
        override_clears,
        override_blocks,
        stats,
    })
}

/// Railings must not cut a deck: where a deck meets land or another deck (within a block)
/// it has no railing, and where its railings split the cells left to walk on (a narrow
/// deck stepping sideways, a gangway as wide as its two railings), the fewest railings
/// that join them again are left out.
fn open_walkways(
    cells: &Cells,
    dcells: &[DeckCell],
    at: &HashMap<usize, Vec<usize>>,
    railing: &mut BTreeMap<(i32, i32, i32), usize>,
    land: &dyn Fn(usize) -> Option<i32>,
) {
    let top_of = |i: usize| at.get(&i).and_then(|ks| ks.iter().map(|&k| dcells[k].top).max());
    let key = |i: usize, t: i32| {
        let (x, z) = cells.local(i);
        (x, t + 1, z)
    };
    let mut done: std::collections::HashSet<usize> = Default::default();
    let mut starts: Vec<usize> = at.keys().copied().collect();
    starts.sort_unstable();
    for s0 in starts {
        if !done.insert(s0) {
            continue;
        }
        // The deck: deck cells joined within a block.
        let mut comp = vec![s0];
        let mut q = 0;
        while q < comp.len() {
            let c = comp[q];
            q += 1;
            let t = top_of(c).expect("deck cell");
            for n in cells.neighbors(c) {
                if let Some(tn) = top_of(n)
                    && (tn - t).abs() <= 1
                    && done.insert(n)
                {
                    comp.push(n);
                }
            }
        }
        if comp.len() < 2 {
            continue;
        }
        let set: std::collections::HashSet<usize> = comp.iter().copied().collect();
        // Where it is entered: beside land within a block (decks of other heights aside).
        for &c in &comp {
            let t = top_of(c).expect("deck cell");
            if cells.neighbors(c).any(|n| {
                !set.contains(&n)
                    && top_of(n).is_none()
                    && land(n).is_some_and(|g| (g - t).abs() <= 1)
            }) {
                railing.remove(&key(c, t));
            }
        }
        let walk = |c: usize, railing: &BTreeMap<(i32, i32, i32), usize>| {
            !railing.contains_key(&key(c, top_of(c).expect("deck cell")))
        };
        // Join the walkable parts: 0-1 search from one part through railed cells (cost 1)
        // to the nearest other part, clearing the railings on the way.
        loop {
            let open: Vec<usize> = comp.iter().copied().filter(|&c| walk(c, railing)).collect();
            if open.is_empty() {
                break;
            }
            let mut part: std::collections::HashSet<usize> = [open[0]].into_iter().collect();
            let mut q = vec![open[0]];
            while let Some(c) = q.pop() {
                for n in cells.neighbors(c) {
                    if set.contains(&n) && walk(n, railing) && part.insert(n) {
                        q.push(n);
                    }
                }
            }
            if part.len() == open.len() {
                break;
            }
            let mut dist: HashMap<usize, (u32, Option<usize>)> =
                part.iter().map(|&c| (c, (0, None))).collect();
            // Sorted: the search order decides between equally short joins, and a hash
            // set's order changes from run to run.
            let mut seeds: Vec<usize> = part.iter().copied().collect();
            seeds.sort_unstable();
            let mut dq: VecDeque<usize> = seeds.into();
            let mut reached = None;
            while let Some(c) = dq.pop_front() {
                let d = dist[&c].0;
                if !part.contains(&c) && walk(c, railing) {
                    reached = Some(c);
                    break;
                }
                for n in cells.neighbors(c) {
                    if !set.contains(&n) {
                        continue;
                    }
                    let w = u32::from(!walk(n, railing));
                    if dist.get(&n).is_none_or(|&(dn, _)| d + w < dn) {
                        dist.insert(n, (d + w, Some(c)));
                        if w == 0 {
                            dq.push_front(n);
                        } else {
                            dq.push_back(n);
                        }
                    }
                }
            }
            let Some(mut c) = reached else { break };
            while let Some((_, Some(p))) = dist.get(&c).copied() {
                railing.remove(&key(p, top_of(p).expect("deck cell")));
                c = p;
            }
        }
    }
}

#[cfg(test)]
mod tests;
