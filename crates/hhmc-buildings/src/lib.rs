//! Building stage: voxelizes building meshes into a layer for one tile.
//!
//! Per solid (Building or BuildingPart) a vertical ray through every column center is
//! intersected with the solid's triangles; sorted hits pair up into filled intervals. An
//! odd number of hits (non-watertight solid) fills from the ground to the highest hit and
//! is counted. Intervals of all parts are merged per column, so stacked parts form one
//! volume. Heights map to blocks like terrain: block `y` stands for heights
//! `[y − 0.5, y + 0.5)`; the block containing a roof is a full block, a bottom slab or
//! omitted depending on how much of it lies below the roof (0.25 / 0.75 thresholds).
//!
//! The lowest interval of a column is snapped to the ground: if the model reaches below
//! the terrain it starts at the ground block; if it floats up to `foundation_max_blocks`
//! above it, it is extended down (foundation). Only the shell (voxels with a 6-neighbor
//! outside the building) is kept, plus interior floors every `story_blocks`. The top
//! voxel of a column is roof: stairs (facing up-slope) on faces sloped 30–60°, flat-roof
//! blocks below 10°, roof blocks otherwise.

use std::collections::{HashMap, HashSet};

use hhmc_citygml::BuildingMesh;
use hhmc_core::config::Origin;
use hhmc_core::ground::GroundModel;
use hhmc_core::landcfg::BuildingStyle;
use hhmc_core::{BlockState, TILE_SIZE, TileKey};
use palette::color_difference::Ciede2000;
use serde::{Deserialize, Serialize};

/// Bump when the logic below changes.
pub const BUILDINGS_VERSION: u32 = 26;

pub mod facade;

/// Small offset of the ray from the column center, so rays do not run exactly through
/// triangle edges and vertices (which would count a hit twice or not at all).
const JITTER: (f64, f64) = (1.37e-4, 2.71e-4);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TopKind {
    Full,
    Slab,
}

/// A filled block interval of one column, with what the roof looks like at its top.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Interval {
    pub bot: i32,
    pub top: i32,
    pub top_kind: TopKind,
    /// Unit normal of the roof triangle at the top, pointing up.
    pub roof_normal: [f64; 3],
}

/// Voxelized building in world block coordinates: column → sorted, disjoint intervals.
#[derive(Clone, Debug, Default)]
pub struct Voxels {
    pub columns: HashMap<(i32, i32), Vec<Interval>>,
    pub odd_columns: usize,
    /// Surface voxels of non-volume geometry (building installations: lanterns, spire
    /// tips, balconies, dormers), which are not closed solids. Sorted, unique.
    pub surface: Vec<(i32, i32, i32)>,
    /// Surface voxels that fill cavities enclosed by installations (closed towers).
    pub installation_fill: usize,
}

impl Voxels {
    pub fn contains(&self, x: i32, y: i32, z: i32) -> bool {
        self.columns.get(&(x, z)).is_some_and(|iv| iv.iter().any(|i| (i.bot..=i.top).contains(&y)))
    }
}

fn sub(a: [f64; 3], b: [f64; 3]) -> [f64; 3] {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

fn cross(u: [f64; 3], v: [f64; 3]) -> [f64; 3] {
    [u[1] * v[2] - u[2] * v[1], u[2] * v[0] - u[0] * v[2], u[0] * v[1] - u[1] * v[0]]
}

/// Block interval for the heights `[a, b]` (a = −∞ for odd-parity columns).
fn to_blocks(a: f64, b: f64) -> Option<(i32, i32, TopKind)> {
    let r = b.round();
    let f = b - (r - 0.5);
    let (top, kind) = if f < 0.25 {
        (r as i32 - 1, TopKind::Full)
    } else if f < 0.75 {
        (r as i32, TopKind::Slab)
    } else {
        (r as i32, TopKind::Full)
    };
    let bot = if a.is_finite() { a.round() as i32 } else { i32::MIN };
    (top >= bot).then_some((bot, top, kind))
}

/// Cells inside the bounding box of `region` (grown by one) that are not `solid` and not
/// 6-connected to the box's boundary: the cavities that the solid cells enclose there.
fn enclosed(
    region: &HashSet<(i32, i32, i32)>,
    solid: impl Fn(i32, i32, i32) -> bool,
) -> Vec<(i32, i32, i32)> {
    let mut lo = [i32::MAX; 3];
    let mut hi = [i32::MIN; 3];
    for &(x, y, z) in region {
        for (k, c) in [x, y, z].into_iter().enumerate() {
            lo[k] = lo[k].min(c - 1);
            hi[k] = hi[k].max(c + 1);
        }
    }
    let dim = [0, 1, 2].map(|k| (hi[k] - lo[k] + 1) as usize);
    let idx = |x: i32, y: i32, z: i32| {
        ((z - lo[2]) as usize * dim[1] + (y - lo[1]) as usize) * dim[0] + (x - lo[0]) as usize
    };
    // 0 = open, 1 = blocked, 2 = reached from outside.
    let mut cell = vec![0u8; dim[0] * dim[1] * dim[2]];
    for z in lo[2]..=hi[2] {
        for y in lo[1]..=hi[1] {
            for x in lo[0]..=hi[0] {
                if solid(x, y, z) {
                    cell[idx(x, y, z)] = 1;
                }
            }
        }
    }
    let mut stack: Vec<(i32, i32, i32)> = Vec::new();
    for z in lo[2]..=hi[2] {
        for y in lo[1]..=hi[1] {
            for x in lo[0]..=hi[0] {
                let border = x == lo[0]
                    || x == hi[0]
                    || y == lo[1]
                    || y == hi[1]
                    || z == lo[2]
                    || z == hi[2];
                if border && cell[idx(x, y, z)] == 0 {
                    cell[idx(x, y, z)] = 2;
                    stack.push((x, y, z));
                }
            }
        }
    }
    while let Some((x, y, z)) = stack.pop() {
        for (dx, dy, dz) in NEIGHBORS {
            let (nx, ny, nz) = (x + dx, y + dy, z + dz);
            if nx < lo[0] || ny < lo[1] || nz < lo[2] || nx > hi[0] || ny > hi[1] || nz > hi[2] {
                continue;
            }
            let i = idx(nx, ny, nz);
            if cell[i] == 0 {
                cell[i] = 2;
                stack.push((nx, ny, nz));
            }
        }
    }
    let mut out = Vec::new();
    for z in lo[2]..=hi[2] {
        for y in lo[1]..=hi[1] {
            for x in lo[0]..=hi[0] {
                if cell[idx(x, y, z)] == 0 {
                    out.push((x, y, z));
                }
            }
        }
    }
    out
}

/// Triangle / axis-aligned cube overlap (separating axis test after Akenine-Möller).
fn tri_box_overlap(c: [f64; 3], h: f64, tri: &[[f64; 3]; 3]) -> bool {
    let v = tri.map(|p| sub(p, c));
    let e = [sub(v[1], v[0]), sub(v[2], v[1]), sub(v[0], v[2])];
    // The 9 edge × axis cross products.
    for ed in &e {
        for k in 0..3 {
            let mut axis = [0.0; 3];
            axis[k] = 1.0;
            let a = cross(axis, *ed);
            let r = h * (a[0].abs() + a[1].abs() + a[2].abs());
            if r == 0.0 {
                continue;
            }
            let p = v.map(|q| q[0] * a[0] + q[1] * a[1] + q[2] * a[2]);
            if p.iter().copied().fold(f64::INFINITY, f64::min) > r
                || p.iter().copied().fold(f64::NEG_INFINITY, f64::max) < -r
            {
                return false;
            }
        }
    }
    // The box's face normals.
    let outside = (0..3).any(|k| {
        let (mn, mx) = (v[0][k].min(v[1][k]).min(v[2][k]), v[0][k].max(v[1][k]).max(v[2][k]));
        mn > h || mx < -h
    });
    if outside {
        return false;
    }
    // The triangle's plane.
    let n = cross(e[0], e[1]);
    let d = n[0] * v[0][0] + n[1] * v[0][1] + n[2] * v[0][2];
    let r = h * (n[0].abs() + n[1].abs() + n[2].abs());
    d.abs() <= r
}

/// Ray-casts every solid of `m` through the column centers it covers.
pub fn voxelize(m: &BuildingMesh, origin: &Origin) -> Voxels {
    let mut out = Voxels::default();
    let mut per_part: Vec<HashMap<(i32, i32), Vec<Interval>>> = Vec::new();
    for part in &m.parts {
        let mut hits: HashMap<(i32, i32), Vec<(f64, usize)>> = HashMap::new();
        let mut normals = Vec::with_capacity(part.len());
        for (ti, t) in part.iter().enumerate() {
            let [a, b, c] = t.v;
            let mut n = cross(sub(b, a), sub(c, a));
            let len = (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt();
            if len < 1e-12 {
                normals.push([0.0, 0.0, 1.0]);
                continue;
            }
            if n[2] < 0.0 {
                n = [-n[0], -n[1], -n[2]];
            }
            normals.push([n[0] / len, n[1] / len, n[2] / len]);
            if n[2] / len < 1e-9 {
                continue; // vertical: never hit by a vertical ray
            }
            let (min_e, max_e) = (a[0].min(b[0]).min(c[0]), a[0].max(b[0]).max(c[0]));
            let (min_n, max_n) = (a[1].min(b[1]).min(c[1]), a[1].max(b[1]).max(c[1]));
            let x0 = (min_e - origin.e0 as f64 - 0.5).floor() as i32;
            let x1 = (max_e - origin.e0 as f64 - 0.5).ceil() as i32;
            let z0 = (origin.n0 as f64 - max_n - 0.5).floor() as i32;
            let z1 = (origin.n0 as f64 - min_n - 0.5).ceil() as i32;
            // Barycentric setup in (E, N).
            let d = (b[1] - c[1]) * (a[0] - c[0]) + (c[0] - b[0]) * (a[1] - c[1]);
            if d.abs() < 1e-12 {
                continue;
            }
            for z in z0..=z1 {
                for x in x0..=x1 {
                    let pe = origin.e0 as f64 + x as f64 + 0.5 + JITTER.0;
                    let pn = origin.n0 as f64 - z as f64 - 0.5 + JITTER.1;
                    let l1 = ((b[1] - c[1]) * (pe - c[0]) + (c[0] - b[0]) * (pn - c[1])) / d;
                    let l2 = ((c[1] - a[1]) * (pe - c[0]) + (a[0] - c[0]) * (pn - c[1])) / d;
                    let l3 = 1.0 - l1 - l2;
                    if l1 < 0.0 || l2 < 0.0 || l3 < 0.0 {
                        continue;
                    }
                    let h = l1 * a[2] + l2 * b[2] + l3 * c[2];
                    hits.entry((x, z)).or_default().push((h, ti));
                }
            }
        }
        let mut cols: HashMap<(i32, i32), Vec<Interval>> = HashMap::new();
        for (k, mut hs) in hits {
            hs.sort_by(|p, q| p.0.total_cmp(&q.0));
            // Duplicate faces (identical geometry stored twice) would break the parity.
            hs.dedup_by(|p, q| (p.0 - q.0).abs() < 1e-6 && normals[p.1] == normals[q.1]);
            let pairs: Vec<(f64, f64, usize)> = if hs.len() % 2 == 0 {
                hs.chunks(2).map(|p| (p[0].0, p[1].0, p[1].1)).collect()
            } else {
                out.odd_columns += 1;
                let top = hs.last().expect("non-empty");
                vec![(f64::NEG_INFINITY, top.0, top.1)]
            };
            let iv: Vec<Interval> = pairs
                .into_iter()
                .filter_map(|(a, b, t)| {
                    to_blocks(a, b).map(|(bot, top, top_kind)| Interval {
                        bot,
                        top,
                        top_kind,
                        roof_normal: normals[t],
                    })
                })
                .collect();
            if !iv.is_empty() {
                cols.insert(k, iv);
            }
        }
        per_part.push(cols);
    }
    // Union of the parts per column; touching or overlapping intervals merge.
    for cols in per_part {
        for (k, iv) in cols {
            out.columns.entry(k).or_default().extend(iv);
        }
    }
    for iv in out.columns.values_mut() {
        merge_column(iv);
    }
    close_diagonal_gaps(&mut out, m, origin);
    open_geometry(&mut out, m, origin);
    out.surface.sort_unstable();
    out.surface.dedup();
    if out.columns.is_empty() && out.surface.is_empty() && m.bbox[0].is_finite() {
        // Narrower than a block (columns, small kiosks): one column at the center, so every
        // building is present.
        let (e, n) = ((m.bbox[0] + m.bbox[3]) / 2.0, (m.bbox[1] + m.bbox[4]) / 2.0);
        if let Some((bot, top, top_kind)) = to_blocks(m.bbox[2], m.bbox[5]) {
            out.columns.insert(
                origin.block_at(e, n),
                vec![Interval { bot, top, top_kind, roof_normal: [0.0, 0.0, 1.0] }],
            );
        }
    }
    for iv in out.columns.values_mut() {
        merge_column(iv);
    }
    out
}

/// Blocks a triangle touches (conservative), in world block coordinates.
fn touched_blocks(t: &hhmc_citygml::MeshTri, origin: &Origin, out: &mut HashSet<(i32, i32, i32)>) {
    // Block space: x = E − E0, y = H + 0.5 (block y spans [y, y + 1)), z = N0 − N.
    let v = t.v.map(|p| [p[0] - origin.e0 as f64, p[2] + 0.5, origin.n0 as f64 - p[1]]);
    let lo = [0, 1, 2].map(|k| v[0][k].min(v[1][k]).min(v[2][k]).floor() as i32);
    let hi = [0, 1, 2].map(|k| v[0][k].max(v[1][k]).max(v[2][k]).floor() as i32);
    for x in lo[0]..=hi[0] {
        for y in lo[1]..=hi[1] {
            for z in lo[2]..=hi[2] {
                let c = [f64::from(x) + 0.5, f64::from(y) + 0.5, f64::from(z) + 0.5];
                if tri_box_overlap(c, 0.5, &v) {
                    out.insert((x, y, z));
                }
            }
        }
    }
}

const NEIGHBORS: [(i32, i32, i32); 6] =
    [(1, 0, 0), (-1, 0, 0), (0, 1, 0), (0, -1, 0), (0, 0, 1), (0, 0, -1)];

/// Geometry that column ray-casting cannot see:
/// - Installations are open surfaces in general (balconies, lantern arcades): every block
///   a triangle touches becomes a surface voxel, so they leave no see-through gaps.
/// - Solids that are not closed, such as a tower whose walls are single vertical polygons
///   between solid pilasters and whose top is an installation spire: a vertical ray never
///   hits a vertical polygon, so those walls vanish.
///
/// Cavities enclosed by the volume, the installations and the building's polygons that
/// are not backed by the volume are filled, together with those polygons' blocks around
/// them. Closed solids have no unbacked polygons and are unchanged.
fn open_geometry(out: &mut Voxels, m: &BuildingMesh, origin: &Origin) {
    let mut inst: HashSet<(i32, i32, i32)> = HashSet::new();
    for t in m.extras.iter().filter(|t| t.kind == hhmc_citygml::SurfaceKind::Installation) {
        touched_blocks(t, origin, &mut inst);
    }
    let mut sheet: HashSet<(i32, i32, i32)> = HashSet::new();
    for t in m.parts.iter().flatten() {
        touched_blocks(t, origin, &mut sheet);
    }
    let vol = |p: (i32, i32, i32)| out.contains(p.0, p.1, p.2);
    sheet.retain(|&p| !vol(p));
    let unbacked: Vec<(i32, i32, i32)> = sheet
        .iter()
        .copied()
        .filter(|&(x, y, z)| NEIGHBORS.iter().all(|(dx, dy, dz)| !vol((x + dx, y + dy, z + dz))))
        .collect();
    let region: HashSet<(i32, i32, i32)> = inst.iter().copied().chain(unbacked).collect();
    let mut add: HashSet<(i32, i32, i32)> = inst.clone();
    if !region.is_empty() {
        let cavity: HashSet<(i32, i32, i32)> = enclosed(&region, |x, y, z| {
            vol((x, y, z)) || inst.contains(&(x, y, z)) || sheet.contains(&(x, y, z))
        })
        .into_iter()
        .collect();
        out.installation_fill = cavity.len();
        if !cavity.is_empty() {
            // Sheet blocks touching the cavity, edges and corners included, so the
            // corners of the shell are closed too.
            for &(x, y, z) in &sheet {
                let touches = (-1..=1).any(|dx| {
                    (-1..=1).any(|dy| (-1..=1).any(|dz| cavity.contains(&(x + dx, y + dy, z + dz))))
                });
                if touches {
                    add.insert((x, y, z));
                }
            }
            add.extend(cavity);
        }
    }
    out.surface.extend(add);
}

/// A canopy: only its roof (the top block of each column) stays, with posts from its base
/// at the footprint's corners and every `spacing` blocks along its edges (by world
/// position, so posts match across tile edges).
fn canopy(v: &mut Voxels, spacing: i32) {
    let keys: HashSet<(i32, i32)> = v.columns.keys().copied().collect();
    for (&(x, z), iv) in v.columns.iter_mut() {
        let (Some(first), Some(&last)) = (iv.first().map(|i| i.bot), iv.last()) else { continue };
        let out = |dx: i32, dz: i32| !keys.contains(&(x + dx, z + dz));
        let (ew, ns) = (out(1, 0) || out(-1, 0), out(0, 1) || out(0, -1));
        let post = (ew && ns) || ((ew || ns) && (x + z).rem_euclid(spacing.max(1)) == 0);
        *iv = vec![Interval { bot: if post { first } else { last.top }, ..last }];
    }
    v.surface.clear();
}

/// Merges sorted, possibly overlapping or touching intervals of one column.
fn merge_column(iv: &mut Vec<Interval>) {
    iv.sort_by_key(|i| i.bot);
    let mut merged: Vec<Interval> = Vec::with_capacity(iv.len());
    for i in iv.drain(..) {
        match merged.last_mut() {
            Some(m) if i.bot <= m.top.saturating_add(1) => {
                if i.top > m.top || (i.top == m.top && i.top_kind == TopKind::Full) {
                    m.top = i.top;
                    m.top_kind = i.top_kind;
                    m.roof_normal = i.roof_normal;
                }
            }
            _ => merged.push(i),
        }
    }
    *iv = merged;
}

/// Thin solids that run diagonally to the grid (walls of rotated towers) come out of
/// column-center sampling as staircases whose voxels touch only at their edges, and
/// light passes through those edges in the game. Wherever two diagonal neighbors are
/// solid at a height and both cells between them are empty, the one nearer the
/// building's center is filled, so every wall is closed side to side.
fn close_diagonal_gaps(v: &mut Voxels, m: &BuildingMesh, origin: &Origin) {
    let (ce, cn) = ((m.bbox[0] + m.bbox[3]) / 2.0, (m.bbox[1] + m.bbox[4]) / 2.0);
    let (cx, cz) = (ce - origin.e0 as f64 - 0.5, origin.n0 as f64 - cn - 0.5);
    let dist = |x: i32, z: i32| (f64::from(x) - cx).hypot(f64::from(z) - cz);
    let ranges = |x: i32, z: i32| -> Vec<(i32, i32)> {
        v.columns
            .get(&(x, z))
            .map_or_else(Vec::new, |iv| iv.iter().map(|i| (i.bot, i.top)).collect())
    };
    let mut add: HashMap<(i32, i32), Vec<(i32, i32)>> = HashMap::new();
    let mut keys: Vec<(i32, i32)> = v.columns.keys().copied().collect();
    keys.sort_unstable();
    for (x, z) in keys {
        let a = ranges(x, z);
        for dz in [1, -1] {
            let b = ranges(x + 1, z + dz);
            if b.is_empty() {
                continue;
            }
            let (c1, c2) = ((x + 1, z), (x, z + dz));
            let blocked: Vec<(i32, i32)> =
                ranges(c1.0, c1.1).into_iter().chain(ranges(c2.0, c2.1)).collect();
            let target = if dist(c2.0, c2.1) < dist(c1.0, c1.1) { c2 } else { c1 };
            for &(a0, a1) in &a {
                for &(b0, b1) in &b {
                    let (lo, hi) = (a0.max(b0), a1.min(b1));
                    if lo > hi {
                        continue;
                    }
                    for r in subtract((lo, hi), &blocked) {
                        add.entry(target).or_default().push(r);
                    }
                }
            }
        }
    }
    for (k, rs) in add {
        let iv = v.columns.entry(k).or_default();
        iv.extend(rs.into_iter().map(|(bot, top)| Interval {
            bot,
            top,
            top_kind: TopKind::Full,
            roof_normal: [0.0, 0.0, 1.0],
        }));
        merge_column(iv);
    }
}

/// `r` minus the union of `cut` (inclusive ranges).
fn subtract(r: (i32, i32), cut: &[(i32, i32)]) -> Vec<(i32, i32)> {
    let mut out = vec![r];
    for &(c0, c1) in cut {
        out = out
            .into_iter()
            .flat_map(|(a, b)| {
                if c1 < a || c0 > b {
                    vec![(a, b)]
                } else {
                    let mut v = Vec::new();
                    if c0 > a {
                        v.push((a, c0 - 1));
                    }
                    if c1 < b {
                        v.push((c1 + 1, b));
                    }
                    v
                }
            })
            .collect();
    }
    out
}

/// Snaps each column's lowest interval to the ground: `ground(x, z)` is the y of the top
/// ground block, or `None` where unknown (outside the tile's read box).
pub fn snap_to_ground(
    v: &mut Voxels,
    ground: &dyn Fn(i32, i32) -> Option<i32>,
    foundation_max: i32,
) {
    for (&(x, z), iv) in v.columns.iter_mut() {
        let Some(g) = ground(x, z) else {
            if let Some(first) = iv.first_mut()
                && first.bot == i32::MIN
            {
                first.bot = first.top;
            }
            continue;
        };
        let first = &mut iv[0];
        if first.bot <= g + foundation_max {
            first.bot = g;
        }
        iv.retain(|i| i.top >= i.bot);
    }
    v.columns.retain(|_, iv| !iv.is_empty());
}

/// Blocks of one building layer, in tile-local coordinates.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct BuildingLayer {
    pub palette: Vec<BlockState>,
    /// (x, z, y0, y1): cleared to air before blocks are placed (building interiors).
    pub clears: Vec<(u16, u16, i16, i16)>,
    /// (x, y, z, palette index).
    pub blocks: Vec<(u16, i16, u16, u16)>,
    pub stats: Vec<BuildingStat>,
    /// Buildings in this tile's bin whose geometry produced no column at all.
    pub empty: Vec<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct BuildingStat {
    pub id: String,
    pub columns_in_tile: usize,
    pub blocks_in_tile: usize,
    pub odd_columns: usize,
    /// Median over the building's columns of (model underside − ground) in blocks,
    /// before snapping; large magnitudes mean the model floats or is sunk.
    pub raw_base_offset: i32,
    /// Columns (in the tile) whose underside stays above the ground after snapping:
    /// arcades and overhangs, or a building model floating in the air.
    pub floating_columns: usize,
    /// Columns (in the tile) over water.
    pub water_columns: usize,
    /// Median ground y and median underside y (after snapping) of the columns in the tile.
    pub median_ground: i32,
    pub median_bottom: i32,
    /// Range of this building's entries in `BuildingLayer::clears`.
    pub clears: (u32, u32),
    /// Materials came from textures or material colors.
    pub textured: bool,
    /// Wall voxels that became curtain-wall glass, window glass, or a palette block.
    pub curtain_voxels: usize,
    pub window_voxels: usize,
    pub textured_wall_voxels: usize,
    pub max_y: i32,
}

struct Palette {
    states: Vec<BlockState>,
    idx: HashMap<BlockState, u16>,
}

impl Palette {
    fn id(&mut self, s: &BlockState) -> u16 {
        if let Some(&i) = self.idx.get(s) {
            return i;
        }
        self.states.push(s.clone());
        let i = (self.states.len() - 1) as u16;
        self.idx.insert(s.clone(), i);
        i
    }
}

fn parse(s: &str) -> BlockState {
    BlockState::parse(s).unwrap_or_else(|_| BlockState::new(s))
}

/// Roof block for the top voxel of a column.
fn roof_block(style: &BuildingStyle, iv: &Interval) -> BlockState {
    let n = iv.roof_normal;
    let slope = n[2].clamp(-1.0, 1.0).acos().to_degrees();
    if slope >= style.stairs_min_deg && slope <= style.stairs_max_deg {
        // The normal leans toward the lower side; stairs face up-slope (+x east, −z north).
        let (ue, un) = (-n[0], -n[1]);
        let facing = if ue.abs() >= un.abs() {
            if ue > 0.0 { "east" } else { "west" }
        } else if un > 0.0 {
            "north"
        } else {
            "south"
        };
        return parse(&style.roof_stairs)
            .with("facing", facing)
            .with("half", "bottom")
            .with("shape", "straight")
            .with("waterlogged", "false");
    }
    let (full, slab) = if slope < style.flat_max_deg {
        (&style.flat_roof, &style.flat_roof_slab)
    } else {
        (&style.roof, &style.roof_slab)
    };
    match iv.top_kind {
        TopKind::Full => parse(full),
        TopKind::Slab => parse(slab).with("type", "bottom").with("waterlogged", "false"),
    }
}

/// Horizontal neighbors: +x (east), −x, +z (south), −z (north).
const SIDES: [(i32, i32); 4] = [(1, 0), (-1, 0), (0, 1), (0, -1)];

#[derive(Clone, Copy, Debug)]
enum Role {
    /// Top voxel of the column's k-th interval.
    Roof(usize),
    Wall,
    /// Bottom voxel of an interval (ground slab): colored like a wall, never glass.
    Base,
    Floor,
}

#[derive(Clone, Copy, Debug)]
struct Vox {
    x: i32,
    y: i32,
    z: i32,
    /// Bit i set: neighbor `SIDES[i]` is outside the building.
    sides: u8,
    role: Role,
}

/// Blocks for one textured building's voxels: palette colors, windows and curtain walls.
#[allow(clippy::too_many_arguments)]
fn textured_blocks(
    f: &facade::Facade,
    sf: &facade::Surface,
    m: &BuildingMesh,
    origin: &Origin,
    v: &Voxels,
    voxels: &[Vox],
    base: i32,
    style: &BuildingStyle,
) -> Vec<BlockState> {
    use facade::FaceSample;
    let images = facade::Images::load(m, &f.root);
    let center = |vx: &Vox| {
        [origin.e0 as f64 + vx.x as f64 + 0.5, origin.n0 as f64 - vx.z as f64 - 0.5, vx.y as f64]
    };
    // Per voxel: samples of its exposed side faces (or its top face for roofs).
    let samples: Vec<Vec<FaceSample>> = voxels
        .iter()
        .map(|vx| {
            let c = center(vx);
            match vx.role {
                Role::Roof(_) => sf
                    .sample(&images, [c[0], c[1], c[2] + 0.5], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0])
                    .into_iter()
                    .collect(),
                Role::Wall | Role::Base => SIDES
                    .iter()
                    .enumerate()
                    .filter(|(bit, _)| vx.sides & (1 << bit) != 0)
                    .filter_map(|(_, &(dx, dz))| {
                        // World +z is south, i.e. −N.
                        let (ne, nn) = (f64::from(dx), -f64::from(dz));
                        let p = [c[0] + 0.5 * ne, c[1] + 0.5 * nn, c[2]];
                        sf.sample(&images, p, [-nn, ne, 0.0], [0.0, 0.0, 1.0])
                    })
                    .collect(),
                Role::Floor => Vec::new(),
            }
        })
        .collect();
    // Per source face: median color, for window and curtain-wall detection.
    let mut per_poly: HashMap<u32, Vec<[f32; 3]>> = HashMap::new();
    for s in samples.iter().flatten() {
        per_poly.entry(s.poly).or_default().push([s.lab.l, s.lab.a, s.lab.b]);
    }
    let median: HashMap<u32, palette::Lab> = per_poly
        .into_iter()
        .map(|(p, mut v)| {
            let med = |k: usize, v: &mut Vec<[f32; 3]>| {
                v.sort_by(|a, b| a[k].total_cmp(&b[k]));
                v[v.len() / 2][k]
            };
            (p, palette::Lab::new(med(0, &mut v), med(1, &mut v), med(2, &mut v)))
        })
        .collect();
    if std::env::var("HHMC_DEBUG_BUILDING").is_ok_and(|id| id == m.id) {
        let mut polys: Vec<_> = median.iter().collect();
        polys.sort_by_key(|(p, _)| **p);
        for (p, md) in polys {
            let (kind, area) = m
                .polys
                .get(*p as usize)
                .copied()
                .unwrap_or((hhmc_citygml::SurfaceKind::Unclassified, 0.0));
            let n = m
                .parts
                .iter()
                .flatten()
                .find(|t| t.poly == *p)
                .map(|t| {
                    let (a, b, c) = (t.v[0], t.v[1], t.v[2]);
                    let u = [b[0] - a[0], b[1] - a[1], b[2] - a[2]];
                    let w = [c[0] - a[0], c[1] - a[1], c[2] - a[2]];
                    let n = [
                        u[1] * w[2] - u[2] * w[1],
                        u[2] * w[0] - u[0] * w[2],
                        u[0] * w[1] - u[1] * w[0],
                    ];
                    let l = (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt();
                    [n[0] / l, n[1] / l, n[2] / l]
                })
                .unwrap_or([0.0; 3]);
            eprintln!(
                "poly {p} {kind:?} area {area:.0} L {:.1} a {:.1} b {:.1} normal E {:.2} N {:.2} up {:.2}",
                md.l, md.a, md.b, n[0], n[1], n[2]
            );
        }
    }
    // Glazed building: dark glass-like samples on enough facing directions.
    let ccfg = &f.cfg.curtain_wall;
    let mut per_side = [(0usize, 0usize); 4];
    for (vx, ss) in voxels.iter().zip(&samples) {
        if !matches!(vx.role, Role::Wall) {
            continue;
        }
        let dirs = (0..4).filter(|bit| vx.sides & (1 << bit) != 0);
        for (bit, s) in dirs.zip(ss) {
            per_side[bit].1 += 1;
            if facade::is_glass_like(ccfg, s.lab) {
                per_side[bit].0 += 1;
            }
        }
    }
    let glazed = per_side
        .iter()
        .filter(|(g, n)| *n > 0 && *g as f32 >= ccfg.min_side_fraction * *n as f32)
        .count()
        >= ccfg.min_sides;
    // Illumination correction toward the building's lit faces (see facade.toml).
    let icfg = &f.cfg.illumination;
    let reference = {
        // Median color of the brightest wall samples: the material in sunlight.
        let is_wall = |p: u32| {
            m.polys.get(p as usize).is_some_and(|(k, _)| *k == hhmc_citygml::SurfaceKind::Wall)
        };
        let mut wall: Vec<palette::Lab> =
            samples.iter().flatten().filter(|s| is_wall(s.poly)).map(|s| s.lab).collect();
        wall.sort_by(|a, b| a.l.total_cmp(&b.l));
        let from = ((wall.len() as f32) * icfg.reference_percentile) as usize;
        let mut top: Vec<palette::Lab> = wall[from.min(wall.len())..].to_vec();
        if top.is_empty() {
            None
        } else {
            let med = |k: usize, v: &mut Vec<palette::Lab>| {
                v.sort_by(|a, b| [a.l, a.a, a.b][k].total_cmp(&[b.l, b.a, b.b][k]));
                let c = v[v.len() / 2];
                [c.l, c.a, c.b][k]
            };
            Some(palette::Lab::<palette::white_point::D65, f32>::new(
                med(0, &mut top),
                med(1, &mut top),
                med(2, &mut top),
            ))
        }
    };
    let correct_lab = |lab: palette::Lab, poly: u32| -> palette::Lab {
        let c = match (icfg.enabled, reference, median.get(&poly)) {
            (true, Some(r), Some(md)) if md.l < r.l => {
                let dl = r.l - md.l;
                let w = (dl / icfg.chroma_full_at).min(1.0);
                palette::Lab::new(
                    lab.l + dl * icfg.lightness_gain,
                    lab.a + (r.a - md.a) * w,
                    lab.b + (r.b - md.b) * w,
                )
            }
            _ => lab,
        };
        palette::Lab::new(c.l, c.a * icfg.chroma_gain, c.b * icfg.chroma_gain)
    };
    let correct = |s: &FaceSample| correct_lab(s.lab, s.poly);
    // Dominant (corrected median) color per face.
    let face_color: HashMap<u32, palette::Lab> =
        median.iter().map(|(p, md)| (*p, correct_lab(*md, *p))).collect();
    let wcfg = &f.cfg.windows;
    // Window candidates repeat per story in the same column of the same face.
    let mut stories: HashMap<(u32, i32, i32), std::collections::HashSet<i32>> = HashMap::new();
    for (vx, ss) in voxels.iter().zip(&samples) {
        for s in ss {
            if matches!(vx.role, Role::Wall)
                && median.get(&s.poly).is_some_and(|md| s.lab.l < md.l - wcfg.delta_l)
            {
                stories
                    .entry((s.poly, vx.x, vx.z))
                    .or_default()
                    .insert((vx.y - base).div_euclid(style.story_blocks));
            }
        }
    }
    voxels
        .iter()
        .zip(&samples)
        .map(|(vx, ss)| match vx.role {
            Role::Floor => parse(&style.floor),
            Role::Roof(k) => {
                let iv = &v.columns[&(vx.x, vx.z)][k];
                match ss.first() {
                    Some(s) => roof_block_family(style, iv, f.nearest_roof(s.lab)),
                    None => roof_block(style, iv),
                }
            }
            Role::Wall | Role::Base => {
                let base = matches!(vx.role, Role::Base);
                if !base && glazed && ss.iter().any(|s| facade::is_glass_like(ccfg, s.lab)) {
                    return parse(&ccfg.block);
                }
                let window = !base
                    && ss.iter().any(|s| {
                        median.get(&s.poly).is_some_and(|md| s.lab.l < md.l - wcfg.delta_l)
                            && stories
                                .get(&(s.poly, vx.x, vx.z))
                                .is_some_and(|st| st.len() >= wcfg.min_stories)
                    });
                if window {
                    return parse(&wcfg.block);
                }
                if ss.is_empty() || reference.is_none_or(|r| r.l < icfg.min_reference_l) {
                    return parse(&style.wall);
                }
                let n = ss.len() as f32;
                let (l, a, b) = ss.iter().fold((0.0, 0.0, 0.0), |acc, s| {
                    let c = correct(s);
                    (acc.0 + c.l, acc.1 + c.a, acc.2 + c.b)
                });
                let own = palette::Lab::new(l / n, a / n, b / n);
                // Snap to the face's dominant color unless the voxel clearly differs.
                let color = match face_color.get(&ss[0].poly) {
                    Some(fc) if own.difference(*fc) <= icfg.face_snap_de => *fc,
                    _ => own,
                };
                parse(f.nearest_wall(color))
            }
        })
        .collect()
}

/// Roof block from a color-matched family: stairs on 30–60° faces, else full or slab.
fn roof_block_family(style: &BuildingStyle, iv: &Interval, fam: &facade::RoofFamily) -> BlockState {
    let s = BuildingStyle {
        flat_roof: fam.full.clone(),
        flat_roof_slab: fam.slab.clone(),
        roof: fam.full.clone(),
        roof_slab: fam.slab.clone(),
        roof_stairs: fam.stairs.clone(),
        ..style.clone()
    };
    roof_block(&s, iv)
}

/// Voxelizes `meshes` into the layer for `tile`. Buildings are voxelized whole, so blocks
/// near tile edges are identical in both tiles; only blocks inside the tile are emitted.
#[allow(clippy::too_many_arguments)]
pub fn building_layer(
    meshes: &[BuildingMesh],
    tile: TileKey,
    origin: &Origin,
    ground: &GroundModel,
    style: &BuildingStyle,
    facade: Option<&facade::Facade>,
    ortho: Option<&hhmc_core::raster::Grid<[u8; 4]>>,
    floating: Option<&[i16]>,
) -> BuildingLayer {
    let mut pal = Palette { states: Vec::new(), idx: HashMap::new() };
    let (wall_state, floor_state) = (parse(&style.wall), parse(&style.floor));
    let wall = pal.id(&wall_state);
    let floor = pal.id(&floor_state);
    let mut layer = BuildingLayer::default();
    let (tx, tz) = tile.min_block();
    let border = ((ground.w - TILE_SIZE as usize) / 2) as i32;
    let ground_at = |x: i32, z: i32| -> Option<i32> {
        let (lx, lz) = (x - tx + border, z - tz + border);
        (lx >= 0 && lz >= 0 && (lx as usize) < ground.w && (lz as usize) < ground.h)
            .then(|| i32::from(ground.ground[lz as usize * ground.w + lx as usize]))
    };
    let in_tile =
        |x: i32, z: i32| (tx..tx + TILE_SIZE).contains(&x) && (tz..tz + TILE_SIZE).contains(&z);
    for m in meshes {
        let mut v = voxelize(m, origin);
        let mut offsets: Vec<i32> = v
            .columns
            .iter()
            .filter_map(|(&(x, z), iv)| {
                Some(iv[0].bot).filter(|&b| b != i32::MIN).zip(ground_at(x, z)).map(|(b, g)| b - g)
            })
            .collect();
        offsets.sort_unstable();
        snap_to_ground(&mut v, &ground_at, style.foundation_max_blocks);
        // Buildings on floating landing stages stand on the deck (`floating`: the deck's
        // top per tile column): LoD2 models them down into the water.
        if let Some(f) = floating {
            let deck = |x: i32, z: i32| {
                in_tile(x, z)
                    .then(|| f[(z - tz) as usize * TILE_SIZE as usize + (x - tx) as usize])
                    .filter(|&t| t != i16::MIN)
                    .map(i32::from)
            };
            for (&(x, z), iv) in v.columns.iter_mut() {
                if let Some(t) = deck(x, z) {
                    iv.retain(|i| i.top > t);
                    for i in iv.iter_mut() {
                        i.bot = i.bot.max(t + 1);
                    }
                }
            }
            v.columns.retain(|_, iv| !iv.is_empty());
            v.surface.retain(|&(x, y, z)| deck(x, z).is_none_or(|t| y > t));
        }
        if m.function.as_ref().is_some_and(|f| style.canopy_functions.contains(f)) {
            canopy(&mut v, style.canopy_post_spacing);
        }
        if v.columns.is_empty() {
            layer.empty.push(m.id.clone());
            continue;
        }
        let base = v.columns.values().map(|iv| iv[0].bot).min().unwrap_or(0);
        let mut stat = BuildingStat {
            id: m.id.clone(),
            odd_columns: v.odd_columns,
            raw_base_offset: offsets.get(offsets.len() / 2).copied().unwrap_or(0),
            max_y: v.columns.values().flat_map(|iv| iv.iter().map(|i| i.top)).max().unwrap_or(0),
            ..Default::default()
        };
        let mut keys: Vec<&(i32, i32)> =
            v.columns.keys().filter(|(x, z)| in_tile(*x, *z)).collect();
        keys.sort();
        let clear_start = layer.clears.len() as u32;
        let (mut gs, mut bs) = (Vec::new(), Vec::new());
        let mut voxels: Vec<Vox> = Vec::new();
        for &(x, z) in keys {
            let iv = &v.columns[&(x, z)];
            stat.columns_in_tile += 1;
            if ground_at(x, z).is_some_and(|g| iv[0].bot > g) {
                stat.floating_columns += 1;
            }
            if let Some(g) = ground_at(x, z) {
                gs.push(g);
                let (lx, lz) = ((x - tx + border) as usize, (z - tz + border) as usize);
                if ground.water_top[lz * ground.w + lx] != hhmc_core::ground::NO_WATER {
                    stat.water_columns += 1;
                }
            }
            bs.push(iv[0].bot);
            let (lx, lz) = ((x - tx) as u16, (z - tz) as u16);
            for (k, i) in iv.iter().enumerate() {
                layer.clears.push((lx, lz, i.bot as i16, i.top as i16));
                for y in i.bot..=i.top {
                    let mut sides = 0u8;
                    for (bit, (dx, dz)) in SIDES.iter().enumerate() {
                        if !v.contains(x + dx, y, z + dz) {
                            sides |= 1 << bit;
                        }
                    }
                    let role = if y == i.top {
                        Role::Roof(k)
                    } else if y == i.bot {
                        Role::Base
                    } else if sides != 0 {
                        Role::Wall
                    } else if (y - base).rem_euclid(style.story_blocks) == 0 {
                        Role::Floor
                    } else {
                        continue;
                    };
                    voxels.push(Vox { x, y, z, sides, role });
                }
            }
        }
        // Installation surfaces outside the volume and above the ground (the ground block
        // itself stays: an installation at ground height is a terrace or step).
        // Only exposed ones get a block: the sides facing air are sampled; voxels exposed
        // only above or below sample all four sides.
        let in_surface: HashSet<(i32, i32, i32)> = v.surface.iter().copied().collect();
        let solid = |x: i32, y: i32, z: i32| v.contains(x, y, z) || in_surface.contains(&(x, y, z));
        for &(x, y, z) in &v.surface {
            if !in_tile(x, z) || v.contains(x, y, z) || ground_at(x, z).is_some_and(|g| y <= g) {
                continue;
            }
            layer.clears.push(((x - tx) as u16, (z - tz) as u16, y as i16, y as i16));
            let mut sides = 0u8;
            for (bit, (dx, dz)) in SIDES.iter().enumerate() {
                if !solid(x + dx, y, z + dz) {
                    sides |= 1 << bit;
                }
            }
            if sides == 0 {
                if solid(x, y + 1, z) && solid(x, y - 1, z) {
                    continue;
                }
                sides = 0b1111;
            }
            voxels.push(Vox { x, y, z, sides, role: Role::Wall });
        }
        // Materials: from textures/material colors where the building has them.
        let surface = facade.and_then(|_| facade::Surface::new(m));
        let chosen: Vec<BlockState> = match (facade, &surface) {
            (Some(f), Some(sf)) => textured_blocks(f, sf, m, origin, &v, &voxels, base, style),
            _ => voxels
                .iter()
                .map(|vx| match vx.role {
                    Role::Roof(k) => {
                        let iv = &v.columns[&(vx.x, vx.z)][k];
                        // Untextured buildings: roof color from the orthophoto at the column.
                        let px = ortho.zip(facade).and_then(|(o, f)| {
                            let e = origin.e0 as f64 + vx.x as f64 + 0.5;
                            let n = origin.n0 as f64 - vx.z as f64 - 0.5;
                            o.at(e, n).filter(|p| p[3] >= 128).map(|p| (f, p))
                        });
                        match px {
                            Some((f, p)) => roof_block_family(
                                style,
                                iv,
                                f.nearest_roof(facade::lab_of([p[0], p[1], p[2]])),
                            ),
                            None => roof_block(style, iv),
                        }
                    }
                    Role::Wall | Role::Base => parse(&style.wall),
                    Role::Floor => parse(&style.floor),
                })
                .collect(),
        };
        if surface.is_some() {
            stat.textured = true;
            for (vx, st) in voxels.iter().zip(&chosen) {
                if matches!(vx.role, Role::Wall | Role::Base) {
                    if facade.is_some_and(|f| st.name == parse(&f.cfg.curtain_wall.block).name) {
                        stat.curtain_voxels += 1;
                    } else if facade.is_some_and(|f| st.name == parse(&f.cfg.windows.block).name) {
                        stat.window_voxels += 1;
                    } else {
                        stat.textured_wall_voxels += 1;
                    }
                }
            }
        }
        for (vx, st) in voxels.iter().zip(chosen) {
            let id = if st == wall_state {
                wall
            } else if st == floor_state {
                floor
            } else {
                pal.id(&st)
            };
            layer.blocks.push(((vx.x - tx) as u16, vx.y as i16, (vx.z - tz) as u16, id));
            stat.blocks_in_tile += 1;
        }
        if stat.columns_in_tile > 0 {
            gs.sort_unstable();
            bs.sort_unstable();
            stat.median_ground = gs.get(gs.len() / 2).copied().unwrap_or(0);
            stat.median_bottom = bs[bs.len() / 2];
            stat.clears = (clear_start, layer.clears.len() as u32);
            layer.stats.push(stat);
        }
    }
    layer.palette = pal.states;
    layer
}

#[cfg(test)]
mod tests;

/// Reads the prepared meshes of `tile` (`data/prepared/<source>/<rx>_<rz>.bin`).
pub fn load_meshes(path: &std::path::Path) -> anyhow::Result<Vec<BuildingMesh>> {
    let raw = zstd_decode(&std::fs::read(path)?)?;
    Ok(postcard::from_bytes(&raw)?)
}

fn zstd_decode(b: &[u8]) -> anyhow::Result<Vec<u8>> {
    Ok(zstd::decode_all(b)?)
}
