//! CPU perspective renderer for QA viewpoints: rays are marched through the voxels of the
//! written region files (Amanatides–Woo traversal), colored with the block palette,
//! shaded per face like the game (top bright, sides darker), with water and glass
//! blended semi-transparently and distance fog. No game client needed.

use std::collections::HashMap;
use std::path::Path;

use anyhow::Result;
use hhmc_core::voxel::{MAX_Y_EXCL, MIN_Y};
use hhmc_core::{AIR, TILE_SIZE, TileKey, VoxelTile};
use rayon::prelude::*;
use serde::Deserialize;

use crate::palette::Rgb;

#[derive(Clone, Debug, Deserialize)]
pub struct Viewpoint {
    pub name: String,
    /// Camera position: EPSG:25832 E, N and height in m NHN.
    pub camera: [f64; 3],
    /// Point to look at, same units.
    pub look_at: [f64; 3],
    #[serde(default = "default_fov")]
    pub fov_deg: f64,
}

fn default_fov() -> f64 {
    70.0
}

/// World-space camera: block coordinates (x east, y up, z south) and the look direction.
pub struct Camera {
    pub pos: [f64; 3],
    pub dir: [f64; 3],
    pub fov_deg: f64,
}

impl Camera {
    pub fn from_viewpoint(v: &Viewpoint, origin: &hhmc_core::config::Origin) -> Self {
        let to = |p: [f64; 3]| [p[0] - origin.e0 as f64, p[2], origin.n0 as f64 - p[1]];
        let (a, b) = (to(v.camera), to(v.look_at));
        let d = [b[0] - a[0], b[1] - a[1], b[2] - a[2]];
        let n = (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt();
        Self { pos: a, dir: d.map(|c| c / n), fov_deg: v.fov_deg }
    }

    /// `/tp` command placing a player at the camera with the same view (yaw 0 = south).
    pub fn tp_command(&self) -> String {
        let yaw = (-self.dir[0]).atan2(self.dir[2]).to_degrees();
        let pitch =
            -(self.dir[1].atan2((self.dir[0].powi(2) + self.dir[2].powi(2)).sqrt())).to_degrees();
        format!(
            "/tp @s {:.1} {:.1} {:.1} {:.0} {:.0}",
            self.pos[0],
            self.pos[1] - 1.62,
            self.pos[2],
            yaw,
            pitch
        )
    }
}

/// A loaded tile with render color and material class per palette entry.
type RenderTile = (VoxelTile, Vec<Option<Rgb>>, Vec<u8>);

/// Loaded tiles plus per-tile palette colors.
pub struct World {
    tiles: HashMap<TileKey, RenderTile>,
}

/// Material class for rendering.
const SOLID: u8 = 0;
const WATER: u8 = 1;
const GLASS: u8 = 2;
const PLANT: u8 = 3;

impl World {
    pub fn load(
        region_dir: &Path,
        tiles: &[TileKey],
        colors: &HashMap<String, Rgb>,
    ) -> Result<Self> {
        let loaded: Vec<_> = tiles
            .par_iter()
            .filter(|t| region_dir.join(t.region_file_name()).exists())
            .map(|&t| -> Result<_> {
                let vt = hhmc_world::read_tile(&region_dir.join(t.region_file_name()), t)?;
                let cols: Vec<Option<Rgb>> =
                    vt.palette.states().iter().map(|s| colors.get(&s.name).copied()).collect();
                let kinds: Vec<u8> = vt
                    .palette
                    .states()
                    .iter()
                    .map(|s| {
                        if s.name.ends_with(":water") {
                            WATER
                        } else if s.name.contains("glass") {
                            GLASS
                        } else if s.name.contains("leaves")
                            || s.name.ends_with("grass")
                            || s.name.contains("flower")
                        {
                            PLANT
                        } else {
                            SOLID
                        }
                    })
                    .collect();
                Ok((t, (vt, cols, kinds)))
            })
            .collect::<Result<_>>()?;
        Ok(Self { tiles: loaded.into_iter().collect() })
    }

    #[inline]
    fn block(&self, x: i32, y: i32, z: i32) -> Option<(Rgb, u8)> {
        if !(MIN_Y..MAX_Y_EXCL).contains(&y) {
            return None;
        }
        let k = TileKey::from_block(x, z);
        let (vt, cols, kinds) = self.tiles.get(&k)?;
        let id = vt.get(x.rem_euclid(TILE_SIZE) as usize, y, z.rem_euclid(TILE_SIZE) as usize);
        if id == AIR {
            return None;
        }
        Some((cols[id as usize].unwrap_or([255, 0, 255]), kinds[id as usize]))
    }
}

const SKY: [f64; 3] = [170.0, 200.0, 235.0];

fn cross(a: [f64; 3], b: [f64; 3]) -> [f64; 3] {
    [a[1] * b[2] - a[2] * b[1], a[2] * b[0] - a[0] * b[2], a[0] * b[1] - a[1] * b[0]]
}

fn norm(a: [f64; 3]) -> [f64; 3] {
    let n = (a[0] * a[0] + a[1] * a[1] + a[2] * a[2]).sqrt();
    a.map(|c| c / n)
}

/// Renders an RGB image of `w` × `h` pixels.
pub fn render(world: &World, cam: &Camera, w: usize, h: usize, max_dist: f64) -> Vec<u8> {
    let fwd = cam.dir;
    let right = norm(cross(fwd, [0.0, 1.0, 0.0]));
    let up = cross(right, fwd);
    let tan = (cam.fov_deg.to_radians() / 2.0).tan();
    let aspect = w as f64 / h as f64;
    let mut img = vec![0u8; w * h * 3];
    img.par_chunks_mut(w * 3).enumerate().for_each(|(py, row)| {
        for px in 0..w {
            let sx = (2.0 * (px as f64 + 0.5) / w as f64 - 1.0) * tan * aspect;
            let sy = (1.0 - 2.0 * (py as f64 + 0.5) / h as f64) * tan;
            let d = norm([0, 1, 2].map(|k| fwd[k] + sx * right[k] + sy * up[k]));
            let c = trace(world, cam.pos, d, max_dist);
            for k in 0..3 {
                row[px * 3 + k] = c[k].clamp(0.0, 255.0) as u8;
            }
        }
    });
    img
}

fn trace(world: &World, o: [f64; 3], d: [f64; 3], max_dist: f64) -> [f64; 3] {
    // Block (x, y, z) occupies [x, x+1) × [y − 0.5, y + 0.5) × [z, z+1) in our height
    // convention; shift y by 0.5 so cells are unit cubes.
    let o = [o[0], o[1] + 0.5, o[2]];
    let mut cell = o.map(|c| c.floor() as i32);
    let step = d.map(|c| if c > 0.0 { 1 } else { -1 });
    let t_delta = d.map(|c| if c != 0.0 { (1.0 / c).abs() } else { f64::INFINITY });
    let mut t_max = [0, 1, 2].map(|k| {
        if d[k] > 0.0 {
            ((cell[k] as f64 + 1.0) - o[k]) / d[k]
        } else if d[k] < 0.0 {
            (o[k] - cell[k] as f64) / -d[k]
        } else {
            f64::INFINITY
        }
    });
    let mut color = [0.0; 3];
    let mut transmit = 1.0;
    // Building interiors are unlit in the game: anything seen through glass is darkened.
    let mut indoor = 1.0;
    let mut axis = 1usize;
    let mut t = 0.0;
    while t < max_dist {
        if let Some((rgb, kind)) = world.block(cell[0], cell[1], cell[2]) {
            let shade = match axis {
                1 => 1.0,
                0 => 0.62,
                _ => 0.8,
            };
            let fog = (t / max_dist).powf(1.5).min(1.0);
            let c =
                [0, 1, 2].map(|k| f64::from(rgb[k]) * shade * indoor * (1.0 - fog) + SKY[k] * fog);
            let alpha = match kind {
                WATER => 0.55,
                GLASS => 0.45,
                _ => 1.0,
            };
            for k in 0..3 {
                color[k] += transmit * alpha * c[k];
            }
            transmit *= 1.0 - alpha;
            if kind == GLASS {
                indoor = 0.25;
            }
            if transmit < 0.05 {
                return color;
            }
        }
        axis = if t_max[0] < t_max[1] {
            if t_max[0] < t_max[2] { 0 } else { 2 }
        } else if t_max[1] < t_max[2] {
            1
        } else {
            2
        };
        t = t_max[axis];
        t_max[axis] += t_delta[axis];
        cell[axis] += step[axis];
        if cell[1] < MIN_Y - 1 || cell[1] > MAX_Y_EXCL + 1 {
            break;
        }
    }
    [0, 1, 2].map(|k| color[k] + transmit * SKY[k])
}
