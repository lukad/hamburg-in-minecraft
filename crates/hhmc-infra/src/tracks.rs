//! Track paths: rails on decks, across gaps between deck polygons, and on a smoothed bed on
//! the ground; see config/infra.toml `[rails]`.

use std::collections::{HashMap, HashSet};

use hhmc_core::config::Origin;
use hhmc_core::ground::{GroundModel, NO_WATER};
use hhmc_core::infra::{InfraConfig, Track};

use crate::{Cells, DeckCell, RailCell};

pub(crate) struct TrackInputs<'a> {
    pub tracks: &'a [Track],
    pub origin: &'a Origin,
    pub ground: &'a GroundModel,
    pub cells: &'a Cells,
    /// Drawn deck cells so far.
    pub dcells: &'a [DeckCell],
    pub cfg: &'a InfraConfig,
    pub gap_kind: usize,
    /// bDOM at a cell where no building, canopy or deck hides the track.
    pub bdom: &'a dyn Fn(usize) -> Option<f32>,
    /// Land-use classes a railway viaduct's deck may extend over between its tracks.
    pub viaduct_land: &'a [u8],
}

#[derive(Default)]
pub(crate) struct TrackPlan {
    /// Track paths through the tile (cells inside it).
    pub paths: Vec<(String, Vec<RailCell>)>,
    /// Decks added where tracks cross gaps between deck polygons.
    pub gap_decks: Vec<DeckCell>,
    /// Cells within one cell of a track (buffered grid).
    pub corridor: HashSet<usize>,
    /// Ground track cells and their bed y.
    pub beds: Vec<(usize, i32)>,
    /// Rails: cell, y, shape (the highest track through a cell wins).
    pub rails: Vec<(usize, i32, &'static str)>,
}

/// Steepest climb of a lifted stretch (rise per meter).
const MAX_GRADE: f64 = 0.05;

/// Planning passes at most.
const MAX_PASSES: usize = 6;

/// Path cells either side whose bDOM gives an off-deck track's height.
const BDOM_WINDOW: usize = 5;

#[derive(Clone, Copy, PartialEq)]
enum Base {
    Deck(i32, usize),
    Gap(i32),
    Ground(i32),
    Water,
}

/// Plans all tracks. Elevated stretches lift neighboring tracks too (a viaduct carries
/// several), so planning repeats with the lifted cells as further starts until no more
/// are lifted.
pub(crate) fn plan(inp: &TrackInputs) -> TrackPlan {
    let mut seeds: HashMap<usize, i32> = HashMap::new();
    for _ in 0..MAX_PASSES {
        let (p, lifted) = plan_pass(inp, &seeds);
        if lifted.keys().all(|k| seeds.contains_key(k)) {
            return p;
        }
        seeds.extend(lifted);
    }
    plan_pass(inp, &seeds).0
}

/// One planning pass; `seeds` are cells lifted by an earlier pass (with their height).
fn plan_pass(inp: &TrackInputs, seeds: &HashMap<usize, i32>) -> (TrackPlan, HashMap<usize, i32>) {
    let (g, cells, cfg) = (inp.ground, inp.cells, inp.cfg);
    let (bx, bz) = g.key.min_block();
    let mut deck_top: HashMap<usize, (i32, usize)> = HashMap::new();
    for d in inp.dcells {
        if cfg.deck[d.kind].floating.is_some() {
            continue;
        }
        let e = deck_top.entry(d.cell).or_insert((d.top, d.kind));
        if d.top > e.0 {
            *e = (d.top, d.kind);
        }
    }
    let gap_kind = &cfg.deck[inp.gap_kind];
    let mut plan = TrackPlan::default();
    let mut gap_cells: HashMap<usize, i32> = HashMap::new();
    let mut taken: HashMap<usize, (i32, &'static str)> = HashMap::new();
    let mut tracks: Vec<&Track> = inp.tracks.iter().collect();
    tracks.sort_by(|a, b| (&a.id, a.line[0].0.to_bits()).cmp(&(&b.id, b.line[0].0.to_bits())));
    for t in tracks {
        let path: Vec<(i32, i32)> = grid_path(&t.line, inp.origin);
        // Contiguous runs of the path inside the buffered grid.
        // (cells, starts at the track's first point, ends at its last point)
        let mut runs: Vec<(Vec<usize>, bool, bool)> = Vec::new();
        let mut cur: Vec<usize> = Vec::new();
        let mut cur_start = false;
        for (k, &(x, z)) in path.iter().enumerate() {
            match g.idx_buffered(x - bx, z - bz) {
                Some(i) => {
                    if cur.is_empty() {
                        cur_start = k == 0;
                    }
                    cur.push(i);
                }
                None if !cur.is_empty() => runs.push((std::mem::take(&mut cur), cur_start, false)),
                None => {}
            }
        }
        if !cur.is_empty() {
            runs.push((cur, cur_start, true));
        }
        let mut out_cells: Vec<RailCell> = Vec::new();
        for (run, _, _) in runs {
            let mut base: Vec<Base> = run
                .iter()
                .map(|&i| match deck_top.get(&i) {
                    Some(&(t, k)) => Base::Deck(t, k),
                    None if g.water_top[i] != NO_WATER => Base::Water,
                    None => Base::Ground(i32::from(g.ground[i])),
                })
                .collect();
            // Gaps between decks: interpolate.
            let max_gap = cfg.rails.max_gap_m.round() as usize;
            let mut k = 0;
            while k < base.len() {
                if matches!(base[k], Base::Deck(..)) {
                    k += 1;
                    continue;
                }
                let a = k;
                while k < base.len() && !matches!(base[k], Base::Deck(..)) {
                    k += 1;
                }
                let b = k; // first deck cell after the gap, or the end
                if a == 0 || b == base.len() || b - a > max_gap {
                    continue;
                }
                let (Base::Deck(ta, _), Base::Deck(tb, _)) = (base[a - 1], base[b]) else {
                    continue;
                };
                for (j, item) in base.iter_mut().enumerate().take(b).skip(a) {
                    let f = (j - a + 1) as f64 / (b - a + 1) as f64;
                    let y = (f64::from(ta) + (f64::from(tb) - f64::from(ta)) * f).round() as i32;
                    let elevated = match *item {
                        Base::Water => true,
                        Base::Ground(gy) => y > gy + 1,
                        _ => false,
                    };
                    if elevated {
                        *item = Base::Gap(y);
                    }
                }
            }
            // Tracks above the ground with no deck polygon under them (ramps to tunnel
            // portals, polygons that stop short): the track's own bDOM, a low quantile
            // along the path (catenary masts drop out), where it is clearly above the
            // ground or the water.
            let own: Vec<Option<f32>> = (0..base.len())
                .map(|j| {
                    if !matches!(base[j], Base::Ground(_) | Base::Water) {
                        return None;
                    }
                    let mut v: Vec<f32> = (j.saturating_sub(BDOM_WINDOW)
                        ..(j + BDOM_WINDOW + 1).min(base.len()))
                        .filter(|&q| matches!(base[q], Base::Ground(_) | Base::Water))
                        .filter_map(|q| (inp.bdom)(run[q]))
                        .collect();
                    if v.len() < BDOM_WINDOW {
                        return None;
                    }
                    v.sort_by(f32::total_cmp);
                    Some(v[(v.len() - 1) / 4])
                })
                .collect();
            // Only continuing a deck (a standing train on a ground track lifts bDOM just
            // as much): walk out from each deck end while the track stays elevated.
            let elevated = |j: usize| -> Option<i32> {
                let y = own[j]?.round() as i32;
                match base[j] {
                    // DGM1 omits ramps and troughs (it is bare earth): follow the track's
                    // own height until it meets the ground.
                    Base::Ground(gy) if y > gy => Some(y),
                    Base::Water if y > i32::from(g.water_top[run[j]]) + 1 => Some(y),
                    _ => None,
                }
            };
            // Starts: the track's own deck cells, and elevated cells right beside a deck
            // at their height (a track that begins at a switch just off the polygon).
            let beside_deck = |j: usize| -> bool {
                let Some(y) = elevated(j) else { return false };
                let (c, r) = cells.col_row(run[j]);
                (-3..=3).any(|dr| {
                    (-3..=3).any(|dc| {
                        cells.at(c + dc, r + dr).is_some_and(|n| {
                            deck_top.get(&n).is_some_and(|&(t, _)| (t - y).abs() <= 1)
                                || seeds.get(&n).is_some_and(|&t| (t - y).abs() <= 1)
                        })
                    })
                })
            };
            let mut lift: Vec<(usize, i32)> = Vec::new();
            for j in 0..base.len() {
                // A start beside a deck is kept with the stretch it begins.
                let start = if matches!(base[j], Base::Deck(..)) {
                    None
                } else if beside_deck(j) {
                    Some((j, elevated(j).expect("elevated")))
                } else {
                    continue;
                };
                // A stretch that reaches a deck must meet it at its height.
                for step in [1i64, -1] {
                    let mut q = j as i64 + step;
                    let mut walked: Vec<(usize, i32)> = Vec::new();
                    // Height the stretch has reached so far (where it starts: the deck or
                    // the start cell).
                    let from = match (start, base[j]) {
                        (Some((_, y)), _) => y,
                        (None, Base::Deck(t, _)) => t,
                        _ => unreachable!("starts are decks or beside decks"),
                    };
                    let ok = loop {
                        let last = walked.last().map_or(from, |w| w.1);
                        if q < 0 || q >= base.len() as i64 {
                            // The track's end or the edge of the grid: nothing against it.
                            break true;
                        }
                        let qu = q as usize;
                        match base[qu] {
                            Base::Deck(t, _) | Base::Gap(t) => break (t - last).abs() <= 2,
                            _ => {}
                        }
                        // Something over the track (an overpass, a roof: its bDOM well
                        // above the stretch, or none): carried across at the interpolated
                        // height if the track is seen again within `max_gap_m`.
                        let hidden = |r: usize| match base[r] {
                            Base::Ground(_) | Base::Water => {
                                own[r].is_none_or(|v| v.round() as i32 > last + 2)
                            }
                            _ => false,
                        };
                        if hidden(qu) {
                            let mut r = q;
                            let mut end: Option<(i64, i32)> = None;
                            while r >= 0
                                && r < base.len() as i64
                                && (r - q).unsigned_abs() as usize <= max_gap
                            {
                                let ru = r as usize;
                                if !hidden(ru) {
                                    let reach =
                                        1 + (((r - q).abs() + 1) as f64 * MAX_GRADE).ceil() as i32;
                                    end = match base[ru] {
                                        Base::Deck(t, _) | Base::Gap(t) => Some((r, t)),
                                        _ => elevated(ru).map(|y| (r, y)),
                                    }
                                    .filter(|&(_, y)| (y - last).abs() <= reach);
                                    break;
                                }
                                r += step;
                            }
                            let Some((r, y2)) = end else { break true };
                            let n = (r - q).abs() + 1;
                            for (k, c) in (1..).zip(
                                std::iter::successors(Some(q), |c| Some(c + step))
                                    .take_while(|&c| c != r),
                            ) {
                                let f = k as f64 / n as f64;
                                let y = (f64::from(last) + f64::from(y2 - last) * f).round() as i32;
                                let above = match base[c as usize] {
                                    Base::Ground(gy) => y > gy,
                                    Base::Water => y > i32::from(g.water_top[run[c as usize]]) + 1,
                                    _ => false,
                                };
                                if above {
                                    walked.push((c as usize, y));
                                }
                            }
                            q = r;
                            continue;
                        }
                        let Some(y) = elevated(qu) else { break true };
                        // A railway climbs gently: at most a block per cell, and about a
                        // block per 20 m from where the stretch starts (trees and masts
                        // beside the track do not lift it).
                        let dist = walked.len() as f64 + 1.0;
                        if (y - last).abs() > 1
                            || f64::from((y - from).abs()) > 1.0 + dist * MAX_GRADE
                        {
                            break true;
                        }
                        walked.push((qu, y));
                        q += step;
                    };
                    if ok {
                        lift.extend(walked);
                        lift.extend(start);
                    }
                }
            }
            if let Ok(dbg) = std::env::var("HHMC_DEBUG_TRACK") {
                let (dx, dz) = dbg.split_once(',').expect("x,z");
                let q: (i32, i32) = (dx.parse().expect("x"), dz.parse().expect("z"));
                let pos = run.iter().position(|&i| {
                    let (x, z) = cells.local(i);
                    (x + bx, z + bz) == q
                });
                if let Some(k) = pos {
                    for j in k.saturating_sub(12)..(k + 12).min(run.len()) {
                        let (x, z) = cells.local(run[j]);
                        let b = match base[j] {
                            Base::Deck(t, _) => format!("deck {t}"),
                            Base::Gap(t) => format!("gap {t}"),
                            Base::Ground(gy) => format!("ground {gy}"),
                            Base::Water => "water".into(),
                        };
                        eprintln!(
                            "{} pass{} ({}, {}) {b} own {:?} beside {} lifted {:?}",
                            t.id,
                            seeds.len(),
                            x + bx,
                            z + bz,
                            own[j],
                            beside_deck(j),
                            lift.iter().find(|l| l.0 == j)
                        );
                    }
                }
            }
            for (j, y) in lift {
                base[j] = Base::Gap(y);
            }
            // Smooth elevated stretches without a deck polygon (median along the path).
            let gaps: Vec<Option<i32>> = base
                .iter()
                .map(|b| match b {
                    Base::Gap(y) => Some(*y),
                    _ => None,
                })
                .collect();
            for j in 0..base.len() {
                if gaps[j].is_none() {
                    continue;
                }
                let mut v: Vec<i32> = (j.saturating_sub(3)..(j + 4).min(base.len()))
                    .filter_map(|q| gaps[q])
                    .collect();
                v.sort_unstable();
                base[j] = Base::Gap(v[v.len() / 2]);
            }
            // Ground beds: median ground over the window, within the ground stretch.
            let w = cfg.rails.bed_window;
            let beds: Vec<Option<i32>> = (0..base.len())
                .map(|j| {
                    let Base::Ground(_) = base[j] else { return None };
                    let mut v: Vec<i32> = (j.saturating_sub(w)..(j + w + 1).min(base.len()))
                        .filter_map(|q| match base[q] {
                            Base::Ground(gy) => Some(gy),
                            _ => None,
                        })
                        .collect();
                    v.sort_unstable();
                    Some(v[v.len() / 2])
                })
                .collect();
            // Rails climb at most one block per cell: limit the bed's slope within each
            // ground stretch (forward, then backward).
            let mut beds = beds;
            for j in 1..beds.len() {
                if let (Some(p), Some(b)) = (beds[j - 1], beds[j]) {
                    beds[j] = Some(b.clamp(p - 1, p + 1));
                }
            }
            for j in (0..beds.len().saturating_sub(1)).rev() {
                if let (Some(n), Some(b)) = (beds[j + 1], beds[j]) {
                    beds[j] = Some(b.clamp(n - 1, n + 1));
                }
            }
            let surface = |j: usize| -> Option<i32> {
                match base[j] {
                    Base::Deck(t, _) | Base::Gap(t) => Some(t),
                    Base::Ground(_) => beds[j],
                    Base::Water => None,
                }
            };
            for (j, &i) in run.iter().enumerate() {
                let Some(s) = surface(j) else { continue };
                // Corridor: this cell and its 8 neighbors.
                let (c, r) = cells.col_row(i);
                for dr in -1..=1 {
                    for dc in -1..=1 {
                        if let Some(n) = cells.at(c + dc, r + dr) {
                            plan.corridor.insert(n);
                        }
                    }
                }
                match base[j] {
                    Base::Gap(y) => {
                        for n in std::iter::once(i).chain(cells.neighbors(i)) {
                            if !deck_top.contains_key(&n) {
                                let e = gap_cells.entry(n).or_insert(y);
                                *e = (*e).max(y);
                            }
                        }
                    }
                    Base::Ground(_) => plan.beds.push((i, s)),
                    _ => {}
                }
                let (lx, lz) = cells.local(i);
                if cells.in_tile(i) {
                    let deck = match base[j] {
                        Base::Deck(_, kd) => Some(kd as u8),
                        Base::Gap(_) => Some(inp.gap_kind as u8),
                        _ => None,
                    };
                    out_cells.push(RailCell { x: lx + bx, z: lz + bz, surface: s as i16, deck });
                }
                let world = |q: usize| {
                    let (x, z) = cells.local(run[q]);
                    (x + bx, z + bz)
                };
                let prev = (j > 0).then(|| (world(j - 1), surface(j - 1)));
                let next = (j + 1 < run.len()).then(|| (world(j + 1), surface(j + 1)));
                let shape = rail_shape(world(j), s, prev, next);
                // Where tracks share a cell (switches, crossings), the highest wins: a
                // track lifted onto a viaduct over one left on the ground below it.
                match taken.get(&i) {
                    Some(&(ts, _)) if ts >= s => {}
                    _ => {
                        taken.insert(i, (s, shape));
                    }
                }
            }
        }
        if !out_cells.is_empty() {
            plan.paths.push((t.id.clone(), out_cells));
        }
    }
    // A viaduct is one surface across its tracks: railway land next to a lifted track
    // whose own bDOM is at the same height belongs to the deck too.
    let fill = cfg.rails.viaduct_fill_m.round() as usize;
    let mut frontier: Vec<(usize, i32, usize)> = {
        let mut v: Vec<(usize, i32)> = gap_cells.iter().map(|(&c, &t)| (c, t)).collect();
        v.sort_unstable();
        v.into_iter().map(|(c, t)| (c, t, 0)).collect()
    };
    while !frontier.is_empty() {
        let mut next = Vec::new();
        for (c, top, d) in frontier {
            if d >= fill {
                continue;
            }
            for n in cells.neighbors(c) {
                if gap_cells.contains_key(&n)
                    || deck_top.contains_key(&n)
                    || !inp.viaduct_land.contains(&g.class[n])
                {
                    continue;
                }
                if (inp.bdom)(n).is_some_and(|h| (f64::from(h) - f64::from(top)).abs() <= 1.5) {
                    gap_cells.insert(n, top);
                    next.push((n, top, d + 1));
                }
            }
        }
        next.sort_unstable();
        frontier = next;
    }
    let mut rails: Vec<(usize, i32, &'static str)> =
        taken.iter().map(|(&i, &(s, shape))| (i, s + 1, shape)).collect();
    rails.sort_unstable();
    plan.rails = rails;
    // Track paths report the surface their rail stands on.
    for (_, cs) in &mut plan.paths {
        for c in cs.iter_mut() {
            if let Some(i) = g.idx_buffered(c.x - bx, c.z - bz)
                && let Some(&(s, _)) = taken.get(&i)
            {
                c.surface = s as i16;
            }
        }
    }
    let lifted = gap_cells.clone();
    let mut gaps: Vec<(usize, i32)> = gap_cells.into_iter().collect();
    gaps.sort_unstable();
    plan.gap_decks = gaps
        .into_iter()
        .map(|(cell, top)| DeckCell {
            kind: inp.gap_kind,
            cell,
            top,
            bottom: top - gap_kind.thickness + 1,
            clearance: gap_kind.min_clearance,
        })
        .collect();
    plan.beds.sort_unstable();
    plan.beds.dedup_by_key(|b| b.0);
    (plan, lifted)
}

/// The cells a track passes through, 4-connected (a diagonal step goes through the side
/// cell nearer the line).
pub fn grid_path(line: &[(f64, f64)], origin: &Origin) -> Vec<(i32, i32)> {
    let mut out: Vec<(i32, i32)> = Vec::new();
    for w in line.windows(2).map(|w| (w[0], w[1])).chain(line.last().map(|&p| (p, p))) {
        let (a, b) = w;
        let c = origin.block_at(a.0, a.1);
        let Some(&last) = out.last() else {
            out.push(c);
            continue;
        };
        if c == last {
            continue;
        }
        let (dx, dz) = (c.0 - last.0, c.1 - last.1);
        if dx.abs() + dz.abs() > 1 {
            // Steps of at most one cell each way (points are 0.5 m apart).
            let mid = ((a.0 + b.0) / 2.0 - origin.e0 as f64, origin.n0 as f64 - (a.1 + b.1) / 2.0);
            let c1 = (last.0 + dx.signum(), last.1);
            let c2 = (last.0, last.1 + dz.signum());
            let d =
                |q: (i32, i32)| (f64::from(q.0) + 0.5 - mid.0).hypot(f64::from(q.1) + 0.5 - mid.1);
            out.push(if d(c1) <= d(c2) { c1 } else { c2 });
        }
        out.push(c);
    }
    out
}

/// Minecraft rail shape for a path cell from its neighbors (`prev`, `next`: cell and
/// surface y). +z is south, +x east.
pub fn rail_shape(
    c: (i32, i32),
    s: i32,
    prev: Option<((i32, i32), Option<i32>)>,
    next: Option<((i32, i32), Option<i32>)>,
) -> &'static str {
    let dir = |q: (i32, i32)| -> Option<&'static str> {
        match (q.0 - c.0, q.1 - c.1) {
            (0, -1) => Some("north"),
            (0, 1) => Some("south"),
            (1, 0) => Some("east"),
            (-1, 0) => Some("west"),
            _ => None,
        }
    };
    let dp = prev.and_then(|p| dir(p.0).map(|d| (d, p.1)));
    let dn = next.and_then(|p| dir(p.0).map(|d| (d, p.1)));
    let axis = |d: &str| if d == "north" || d == "south" { "north_south" } else { "east_west" };
    let ascending = |d: &str| match d {
        "north" => "ascending_north",
        "south" => "ascending_south",
        "east" => "ascending_east",
        _ => "ascending_west",
    };
    match (dp, dn) {
        (Some((a, sa)), Some((b, sb))) => {
            if axis(a) == axis(b) && a != b {
                if sb == Some(s + 1) {
                    ascending(b)
                } else if sa == Some(s + 1) {
                    ascending(a)
                } else {
                    axis(a)
                }
            } else {
                let has = |d: &str| a == d || b == d;
                match (has("north"), has("south"), has("east"), has("west")) {
                    (false, true, true, false) => "south_east",
                    (false, true, false, true) => "south_west",
                    (true, false, false, true) => "north_west",
                    (true, false, true, false) => "north_east",
                    _ => axis(a),
                }
            }
        }
        (Some((a, sa)), None) | (None, Some((a, sa))) => {
            if sa == Some(s + 1) {
                ascending(a)
            } else {
                axis(a)
            }
        }
        (None, None) => "north_south",
    }
}
