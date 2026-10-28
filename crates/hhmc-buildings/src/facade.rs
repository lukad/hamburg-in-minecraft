//! Facade materials: texture sampling, window detection and palette matching.
//!
//! For each exposed face of a shell voxel the nearest textured (or colored) triangle of the
//! building is found; 3 × 3 points spread over the 1 × 1 m face are projected onto it,
//! mapped to texture coordinates with barycentric weights and averaged in linear RGB. The
//! result is matched in CIELAB (CIEDE2000) against the facade palette. Texture coordinates
//! follow CityGML: (0, 0) is the lower-left corner of the image.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use hhmc_citygml::BuildingMesh;
use hhmc_core::BlockState;
use palette::color_difference::Ciede2000;
use palette::{IntoColor, Lab, LinSrgb, Srgb};
use serde::Deserialize;

#[derive(Clone, Debug, Deserialize)]
pub struct RoofFamily {
    pub full: String,
    pub stairs: String,
    pub slab: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct WindowsConfig {
    pub delta_l: f32,
    pub min_stories: usize,
    pub block: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct CurtainWallConfig {
    pub max_l: f32,
    pub max_abs_a: f32,
    pub max_b: f32,
    pub max_chroma: f32,
    pub min_sides: usize,
    pub min_side_fraction: f32,
    pub block: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct IlluminationConfig {
    pub enabled: bool,
    pub reference_percentile: f32,
    pub min_reference_l: f32,
    pub lightness_gain: f32,
    pub chroma_full_at: f32,
    pub chroma_gain: f32,
    pub face_snap_de: f32,
}

/// `config/facade.toml`.
#[derive(Clone, Debug, Deserialize)]
pub struct FacadeConfig {
    pub walls: Vec<String>,
    pub roof: Vec<RoofFamily>,
    pub illumination: IlluminationConfig,
    pub windows: WindowsConfig,
    pub curtain_wall: CurtainWallConfig,
}

/// Facade palette with CIELAB colors, plus where texture paths are resolved from.
pub struct Facade {
    pub cfg: FacadeConfig,
    pub root: PathBuf,
    walls: Vec<(String, Lab)>,
    roofs: Vec<(usize, Lab)>,
}

pub fn lab_of(rgb: [u8; 3]) -> Lab {
    Srgb::new(rgb[0], rgb[1], rgb[2]).into_format::<f32>().into_linear::<f32>().into_color()
}

fn lab_of_linear(c: [f32; 3]) -> Lab {
    LinSrgb::new(c[0], c[1], c[2]).into_color()
}

fn srgb_to_linear(v: u8) -> f32 {
    let c = f32::from(v) / 255.0;
    if c <= 0.04045 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }
}

impl Facade {
    /// Loads `config/facade.toml` with colors from `config/block_colors.toml`.
    pub fn load(root: &Path) -> Result<Self> {
        let cfg: FacadeConfig = hhmc_core::config::load_toml(&root.join("config/facade.toml"))?;
        #[derive(Deserialize)]
        struct Colors {
            color: HashMap<String, String>,
        }
        let colors: Colors = hhmc_core::config::load_toml(&root.join("config/block_colors.toml"))?;
        let rgb = |id: &str| -> Result<Lab> {
            let h = colors
                .color
                .get(id)
                .with_context(|| format!("no color for {id} in block_colors.toml"))?;
            let v = u32::from_str_radix(h.trim_start_matches('#'), 16)?;
            Ok(lab_of([(v >> 16) as u8, (v >> 8) as u8, v as u8]))
        };
        let walls = cfg.walls.iter().map(|w| Ok((w.clone(), rgb(w)?))).collect::<Result<_>>()?;
        let roofs = cfg
            .roof
            .iter()
            .enumerate()
            .map(|(i, r)| Ok((i, rgb(&r.full)?)))
            .collect::<Result<_>>()?;
        Ok(Self { cfg, root: root.to_path_buf(), walls, roofs })
    }

    pub fn nearest_wall(&self, c: Lab) -> &str {
        &self
            .walls
            .iter()
            .min_by(|a, b| a.1.difference(c).total_cmp(&b.1.difference(c)))
            .expect("palette not empty")
            .0
    }

    pub fn nearest_roof(&self, c: Lab) -> &RoofFamily {
        let i = self
            .roofs
            .iter()
            .min_by(|a, b| a.1.difference(c).total_cmp(&b.1.difference(c)))
            .expect("palette not empty")
            .0;
        &self.cfg.roof[i]
    }
}

/// Decoded texture images of one building.
pub struct Images {
    images: Vec<Option<(usize, usize, Vec<u8>)>>,
}

impl Images {
    pub fn load(m: &BuildingMesh, root: &Path) -> Self {
        let images = m
            .textures
            .iter()
            .map(|p| {
                let bytes = std::fs::read(root.join(p)).ok()?;
                let mut d = zune_jpeg::JpegDecoder::new(std::io::Cursor::new(bytes));
                let px = d.decode().ok()?;
                let info = d.info()?;
                let (w, h) = (usize::from(info.width), usize::from(info.height));
                (px.len() == w * h * 3).then_some((w, h, px))
            })
            .collect();
        Self { images }
    }

    /// Linear-RGB texel at texture coordinate (u, v), clamped to the image.
    fn texel(&self, tex: u16, uv: [f32; 2]) -> Option<[f32; 3]> {
        let (w, h, px) = self.images.get(tex as usize)?.as_ref()?;
        let x = ((uv[0].clamp(0.0, 1.0) * *w as f32) as usize).min(w - 1);
        let v = if std::env::var_os("HHMC_UV_TOPLEFT").is_some() { uv[1] } else { 1.0 - uv[1] };
        let y = ((v.clamp(0.0, 1.0) * *h as f32) as usize).min(h - 1);
        let i = (y * w + x) * 3;
        Some([srgb_to_linear(px[i]), srgb_to_linear(px[i + 1]), srgb_to_linear(px[i + 2])])
    }
}

struct Tri {
    v: [[f64; 3]; 3],
    tex: Option<(u16, [[f32; 2]; 3])>,
    color: Option<[u8; 3]>,
    poly: u32,
}

/// Textured surface of a building with a uniform-grid index for nearest-triangle queries.
pub struct Surface {
    tris: Vec<Tri>,
    grid: HashMap<(i32, i32, i32), Vec<u32>>,
}

const CELL: f64 = 2.0;

fn cell(p: [f64; 3]) -> (i32, i32, i32) {
    ((p[0] / CELL).floor() as i32, (p[1] / CELL).floor() as i32, (p[2] / CELL).floor() as i32)
}

fn sub(a: [f64; 3], b: [f64; 3]) -> [f64; 3] {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

fn dot(a: [f64; 3], b: [f64; 3]) -> f64 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

/// Closest point on triangle `t` to `p` as barycentric weights (Ericson, RTCD 5.1.5).
fn closest_bary(p: [f64; 3], t: &[[f64; 3]; 3]) -> [f64; 3] {
    let (a, b, c) = (t[0], t[1], t[2]);
    let (ab, ac, ap) = (sub(b, a), sub(c, a), sub(p, a));
    let (d1, d2) = (dot(ab, ap), dot(ac, ap));
    if d1 <= 0.0 && d2 <= 0.0 {
        return [1.0, 0.0, 0.0];
    }
    let bp = sub(p, b);
    let (d3, d4) = (dot(ab, bp), dot(ac, bp));
    if d3 >= 0.0 && d4 <= d3 {
        return [0.0, 1.0, 0.0];
    }
    let vc = d1 * d4 - d3 * d2;
    if vc <= 0.0 && d1 >= 0.0 && d3 <= 0.0 {
        let v = d1 / (d1 - d3);
        return [1.0 - v, v, 0.0];
    }
    let cp = sub(p, c);
    let (d5, d6) = (dot(ab, cp), dot(ac, cp));
    if d6 >= 0.0 && d5 <= d6 {
        return [0.0, 0.0, 1.0];
    }
    let vb = d5 * d2 - d1 * d6;
    if vb <= 0.0 && d2 >= 0.0 && d6 <= 0.0 {
        let w = d2 / (d2 - d6);
        return [1.0 - w, 0.0, w];
    }
    let va = d3 * d6 - d5 * d4;
    if va <= 0.0 && (d4 - d3) >= 0.0 && (d5 - d6) >= 0.0 {
        let w = (d4 - d3) / ((d4 - d3) + (d5 - d6));
        return [0.0, 1.0 - w, w];
    }
    let denom = 1.0 / (va + vb + vc);
    let (v, w) = (vb * denom, vc * denom);
    [1.0 - v - w, v, w]
}

fn point(t: &[[f64; 3]; 3], b: [f64; 3]) -> [f64; 3] {
    [0, 1, 2].map(|k| b[0] * t[0][k] + b[1] * t[1][k] + b[2] * t[2][k])
}

/// One face's color sample.
#[derive(Clone, Copy, Debug)]
pub struct FaceSample {
    pub lab: Lab,
    pub poly: u32,
}

impl Surface {
    /// Volume triangles that carry a texture or a material color; `None` if there are none.
    pub fn new(m: &BuildingMesh) -> Option<Self> {
        let tris: Vec<Tri> = m
            .parts
            .iter()
            .flatten()
            .chain(m.extras.iter())
            .filter(|t| t.tex.is_some() || t.color.is_some())
            .map(|t| Tri { v: t.v, tex: t.tex, color: t.color, poly: t.poly })
            .collect();
        if tris.is_empty() {
            return None;
        }
        let mut grid: HashMap<(i32, i32, i32), Vec<u32>> = HashMap::new();
        for (i, t) in tris.iter().enumerate() {
            let lo =
                cell([0, 1, 2].map(|k| t.v.iter().map(|v| v[k]).fold(f64::INFINITY, f64::min)));
            let hi =
                cell([0, 1, 2].map(|k| t.v.iter().map(|v| v[k]).fold(f64::NEG_INFINITY, f64::max)));
            for x in lo.0..=hi.0 {
                for y in lo.1..=hi.1 {
                    for z in lo.2..=hi.2 {
                        grid.entry((x, y, z)).or_default().push(i as u32);
                    }
                }
            }
        }
        Some(Self { tris, grid })
    }

    fn nearest(&self, p: [f64; 3]) -> Option<(usize, [f64; 3])> {
        let c = cell(p);
        let mut best: Option<(f64, usize, [f64; 3])> = None;
        for r in 1..=3 {
            for x in c.0 - r..=c.0 + r {
                for y in c.1 - r..=c.1 + r {
                    for z in c.2 - r..=c.2 + r {
                        for &i in self.grid.get(&(x, y, z)).into_iter().flatten() {
                            let t = &self.tris[i as usize];
                            let b = closest_bary(p, &t.v);
                            let q = point(&t.v, b);
                            let d = dot(sub(q, p), sub(q, p));
                            if best.is_none_or(|bb| d < bb.0) {
                                best = Some((d, i as usize, b));
                            }
                        }
                    }
                }
            }
            if best.is_some() {
                break;
            }
        }
        best.map(|(_, i, b)| (i, b))
    }

    /// Color of the 1 × 1 m face centered at `p` with in-plane axes `u`, `v` (unit vectors).
    pub fn sample(
        &self,
        images: &Images,
        p: [f64; 3],
        u: [f64; 3],
        v: [f64; 3],
    ) -> Option<FaceSample> {
        let (i, _) = self.nearest(p)?;
        let t = &self.tris[i];
        if let Some(c) = t.color
            && t.tex.is_none()
        {
            return Some(FaceSample { lab: lab_of(c), poly: t.poly });
        }
        let (tex, uv) = t.tex?;
        let mut acc = [0.0f32; 3];
        let mut n = 0.0f32;
        for a in [-1.0 / 3.0, 0.0, 1.0 / 3.0] {
            for b in [-1.0 / 3.0, 0.0, 1.0 / 3.0] {
                let q = [0, 1, 2].map(|k| p[k] + a * u[k] + b * v[k]);
                let w = closest_bary(q, &t.v);
                let st = [0, 1].map(|k| {
                    (w[0] as f32) * uv[0][k] + (w[1] as f32) * uv[1][k] + (w[2] as f32) * uv[2][k]
                });
                if let Some(c) = images.texel(tex, st) {
                    for k in 0..3 {
                        acc[k] += c[k];
                    }
                    n += 1.0;
                }
            }
        }
        (n > 0.0).then(|| FaceSample { lab: lab_of_linear(acc.map(|c| c / n)), poly: t.poly })
    }
}

/// Whether a sampled wall color is dark and glass-like.
pub fn is_glass_like(cfg: &CurtainWallConfig, c: Lab) -> bool {
    c.l <= cfg.max_l
        && c.a.abs() <= cfg.max_abs_a
        && c.b <= cfg.max_b
        && (c.a * c.a + c.b * c.b).sqrt() <= cfg.max_chroma
}

pub fn parse_state(s: &str) -> BlockState {
    BlockState::parse(s).unwrap_or_else(|_| BlockState::new(s))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn closest_point_inside_and_outside() {
        let t = [[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]];
        let b = closest_bary([0.25, 0.25, 5.0], &t);
        assert!((b[1] - 0.25).abs() < 1e-12 && (b[2] - 0.25).abs() < 1e-12);
        assert_eq!(closest_bary([-1.0, -1.0, 0.0], &t), [1.0, 0.0, 0.0]);
        let e = closest_bary([0.5, -2.0, 0.0], &t);
        assert!((e[1] - 0.5).abs() < 1e-12 && e[2] == 0.0);
    }

    #[test]
    fn palette_picks_brick_for_brick_red() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let f = Facade::load(&root).unwrap();
        // Typical Speicherstadt brick in daylight.
        let w = f.nearest_wall(lab_of([150, 85, 70]));
        assert!(w.contains("brick") || w.contains("terracotta") || w.contains("granite"), "{w}");
        let r = f.nearest_roof(lab_of([245, 245, 245]));
        assert!(r.full.contains("white") || r.full.contains("quartz"), "{}", r.full);
    }
}
