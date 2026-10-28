//! Bridge crossings. ALKIS has no land use on bridges (the water runs on under them) and its
//! bridge polygons end a little short of the banks in places, so each connected deck is
//! treated as a crossing: the direction in which its cells reach land at both ends is
//! found; the deck is extended along it over short gaps of water to the banks; and every
//! deck cell takes the surface of the street it lines up with, so lanes and sidewalks run
//! straight across.

use std::collections::HashMap;

use crate::Cells;

/// Directions tried, in degrees (0..180).
const STEP_DEG: usize = 3;
/// A crossing direction's score ((cells landing at both ends, at one end), mean span)
/// and the direction.
type Candidate = ((usize, usize), f64, (f64, f64));

/// Road under a bridge (ALKIS land, ground-model water) is followed this far (m).
const MAX_UNDER_M: f64 = 20.0;
/// Samples per deck when choosing its direction.
const MAX_SAMPLES: usize = 600;

pub(crate) struct CrossingInputs<'a> {
    pub cells: &'a Cells,
    /// The deck kind's cells.
    pub mask: &'a [bool],
    /// Water without any deck that ALKIS also calls water.
    pub open_water: &'a dyn Fn(usize) -> bool,
    /// Water (in the ground model) that ALKIS calls land: the road under a bridge, turned
    /// into water in prep because DGM1 shows the water level there.
    pub under_bridge: &'a dyn Fn(usize) -> bool,
    /// Gaps of water up to this long (m) between a deck end and land are bridged.
    pub max_gap_m: f64,
    /// Whether a land cell is a landing for the deck cell it is reached from (its ground
    /// close to the deck's height there, so an overpass does not land on the road below).
    pub lands: &'a dyn Fn(usize, usize) -> bool,
    /// Cells of other decks (pontoons, other kinds): a deck reaching one lands on it.
    pub other_deck: &'a dyn Fn(usize) -> bool,
}

pub(crate) struct Crossings {
    /// Unit direction (column, row) per cell of a deck that crosses something.
    pub dir: HashMap<usize, (f64, f64)>,
    /// Water cells added between deck ends and the banks.
    pub extension: Vec<usize>,
}

/// Connected decks (4-neighborhood) of `mask`.
fn components(cells: &Cells, mask: &[bool]) -> Vec<Vec<usize>> {
    let mut seen = vec![false; mask.len()];
    let mut out = Vec::new();
    for s in 0..mask.len() {
        if !mask[s] || seen[s] {
            continue;
        }
        let mut comp = vec![s];
        seen[s] = true;
        let mut k = 0;
        while k < comp.len() {
            let i = comp[k];
            k += 1;
            for n in cells.neighbors(i) {
                if mask[n] && !seen[n] {
                    seen[n] = true;
                    comp.push(n);
                }
            }
        }
        comp.sort_unstable();
        out.push(comp);
    }
    out
}

/// Walks from the center of cell `i` along `d` while `inside` holds. Returns the cells
/// passed outside (up to `limit` m beyond the exit) in order, and the distance to the exit.
fn walk(
    cells: &Cells,
    i: usize,
    d: (f64, f64),
    inside: impl Fn(usize) -> bool,
    limit: f64,
) -> (Vec<usize>, f64, usize) {
    let (c0, r0) = cells.col_row(i);
    let (x0, y0) = (c0 as f64 + 0.5, r0 as f64 + 0.5);
    let mut t = 0.0;
    let mut exit: Option<f64> = None;
    let mut last_inside = i;
    let mut out: Vec<usize> = Vec::new();
    loop {
        t += 0.5;
        let (x, y) = (x0 + d.0 * t, y0 + d.1 * t);
        let Some(c) = cells.at(x.floor() as i64, y.floor() as i64) else { break };
        match exit {
            None if inside(c) => {
                last_inside = c;
                continue;
            }
            None => exit = Some(t),
            Some(e) if t - e > limit => break,
            Some(_) => {}
        }
        if out.last() != Some(&c) {
            out.push(c);
        }
    }
    (out, exit.unwrap_or(t), last_inside)
}

/// The first land cell (not open water, not this deck) beyond the deck along `d`, if the
/// water in between is at most `max_gap` m; with the water cells passed.
fn landing(
    inp: &CrossingInputs,
    i: usize,
    d: (f64, f64),
    in_comp: &dyn Fn(usize) -> bool,
) -> Option<(usize, Vec<usize>)> {
    let (outside, _, last) = walk(inp.cells, i, d, in_comp, inp.max_gap_m + MAX_UNDER_M);
    let mut water = Vec::new();
    let mut gap = 0usize;
    for c in outside {
        if in_comp(c) {
            return None; // back on the deck (it curves): no landing this way
        }
        if (inp.other_deck)(c) {
            return Some((c, water));
        }
        if (inp.under_bridge)(c) {
            water.push(c);
            continue;
        }
        if (inp.open_water)(c) {
            gap += 1;
            if gap as f64 > inp.max_gap_m {
                return None;
            }
            water.push(c);
            continue;
        }
        return (inp.lands)(last, c).then_some((c, water));
    }
    None
}

pub(crate) fn crossings(inp: &CrossingInputs) -> Crossings {
    let mut out = Crossings { dir: HashMap::new(), extension: Vec::new() };
    for comp in components(inp.cells, inp.mask) {
        let set: std::collections::HashSet<usize> = comp.iter().copied().collect();
        let in_comp = |c: usize| set.contains(&c);
        let stride = comp.len().div_ceil(MAX_SAMPLES).max(1);
        let samples: Vec<usize> = comp.iter().copied().step_by(stride).collect();
        // The direction in which most cells reach land at both ends, then the shortest.
        // Ranked by cells landing at both ends, then at one end (a pier or a footbridge
        // to a dead end), then the shortest span.
        let mut best: Option<Candidate> = None;
        for deg in (0..180).step_by(STEP_DEG) {
            let a = (deg as f64).to_radians();
            let d = (a.cos(), a.sin());
            let (mut both, mut one, mut span) = (0usize, 0usize, 0.0f64);
            for &i in &samples {
                let f = landing(inp, i, d, &in_comp).is_some();
                let b = landing(inp, i, (-d.0, -d.1), &in_comp).is_some();
                if f && b {
                    both += 1;
                    span += walk(inp.cells, i, d, in_comp, 0.0).1
                        + walk(inp.cells, i, (-d.0, -d.1), in_comp, 0.0).1;
                } else if f || b {
                    one += 1;
                }
            }
            if both + one == 0 {
                continue;
            }
            let mean = if both > 0 { span / both as f64 } else { f64::MAX };
            let better = match best {
                None => true,
                Some((h, m, _)) => (both, one) > h || ((both, one) == h && mean < m),
            };
            if better {
                best = Some(((both, one), mean, d));
            }
        }
        let Some((_, _, d)) = best else { continue };
        let mut ext: Vec<usize> = Vec::new();
        for &i in &comp {
            out.dir.insert(i, d);
            for s in [d, (-d.0, -d.1)] {
                if let Some((_, water)) = landing(inp, i, s, &in_comp) {
                    ext.extend(water);
                }
            }
        }
        ext.sort_unstable();
        ext.dedup();
        for &c in &ext {
            out.dir.insert(c, d);
        }
        out.extension.extend(ext);
    }
    out
}

/// The street class a deck cell lines up with: walking along its crossing direction both
/// ways to the land beyond, the class at the nearer end if it is a surface class, else at
/// the other end.
pub(crate) fn lined_up_class(
    inp: &CrossingInputs,
    i: usize,
    d: (f64, f64),
    class_of: &dyn Fn(usize) -> Option<u8>,
) -> Option<u8> {
    let in_deck = |c: usize| inp.mask[c];
    let mut ends: Vec<(f64, Option<u8>)> = Vec::new();
    for s in [d, (-d.0, -d.1)] {
        if let Some((land, _)) = landing(inp, i, s, &in_deck) {
            let dist = walk(inp.cells, i, s, in_deck, 0.0).1;
            ends.push((dist, class_of(land)));
        }
    }
    ends.sort_by(|a, b| a.0.total_cmp(&b.0));
    ends.into_iter().find_map(|e| e.1)
}
