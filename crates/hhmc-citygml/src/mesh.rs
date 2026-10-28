//! Triangle meshes of buildings: what prep stores per tile and the voxelizer reads.

use serde::{Deserialize, Serialize};

use std::collections::HashMap;

use crate::triangulate::triangulate_indices;
use crate::{Building, SurfaceKind};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MeshTri {
    pub kind: SurfaceKind,
    pub v: [[f64; 3]; 3],
    /// Index of the source polygon within the building (for per-face statistics).
    pub poly: u32,
    /// Texture (index into `BuildingMesh::textures`) and texture coordinates.
    pub tex: Option<(u16, [[f32; 2]; 3])>,
    /// `X3DMaterial` diffuse color, sRGB 0–255.
    pub color: Option<[u8; 3]>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BuildingMesh {
    pub id: String,
    /// Dataset id from data/sources.toml, e.g. `lod2`.
    pub source: String,
    pub function: Option<String>,
    pub measured_height: Option<f64>,
    /// Closed solids (Building and BuildingParts); each is voxelized on its own.
    pub parts: Vec<Vec<MeshTri>>,
    /// [min_e, min_n, min_h, max_e, max_n, max_h].
    pub bbox: [f64; 6],
    /// Texture image paths (resolved by the producer, e.g. relative to the repo root).
    pub textures: Vec<String>,
    /// Polygons that bound the volume, in `MeshTri::poly` order: (kind, area in m²).
    #[serde(default)]
    pub polys: Vec<(SurfaceKind, f32)>,
    /// Non-volume geometry (installations, openings), with the same per-triangle data.
    #[serde(default)]
    pub extras: Vec<MeshTri>,
}

impl BuildingMesh {
    /// Builds the mesh; `resolve` maps a texture `imageURI` to the stored path.
    pub fn from_building_with(
        b: &Building,
        source: &str,
        resolve: &dyn Fn(&str) -> String,
    ) -> Option<Self> {
        // Texture coordinates by ring id, materials by polygon id.
        let mut uv_of: HashMap<&str, (u16, &Vec<[f32; 2]>)> = HashMap::new();
        let mut textures = Vec::new();
        for t in &b.textures {
            let ti = textures.len() as u16;
            textures.push(resolve(&t.image_uri));
            for (ring, uv) in &t.rings {
                uv_of.insert(ring.as_str(), (ti, uv));
            }
        }
        let mut color_of: HashMap<&str, [u8; 3]> = HashMap::new();
        for m in &b.materials {
            let c = m.diffuse.map(|v| (v.clamp(0.0, 1.0) * 255.0).round() as u8);
            for t in &m.targets {
                color_of.insert(t.as_str(), c);
            }
        }
        let mut polys = Vec::new();
        let tris_of = |p: &crate::Polygon, poly: u32| -> Vec<MeshTri> {
            let verts: Vec<[f64; 3]> = p.rings.iter().flatten().copied().collect();
            // Texture coordinates for the concatenated rings, if every ring has them.
            let mut uvs: Vec<[f32; 2]> = Vec::new();
            let mut tex = None;
            for (ring, rid) in p.rings.iter().zip(&p.ring_ids) {
                match rid.as_deref().and_then(|r| uv_of.get(r)) {
                    Some((ti, uv)) if uv.len() >= ring.len() => {
                        tex = Some(*ti);
                        uvs.extend_from_slice(&uv[..ring.len()]);
                    }
                    _ => {
                        tex = None;
                        break;
                    }
                }
            }
            let color = p.id.as_deref().and_then(|id| color_of.get(id)).copied();
            triangulate_indices(p)
                .into_iter()
                .map(|t| MeshTri {
                    kind: p.kind,
                    v: [verts[t[0]], verts[t[1]], verts[t[2]]],
                    poly,
                    tex: tex.map(|ti| (ti, [uvs[t[0]], uvs[t[1]], uvs[t[2]]])),
                    color,
                })
                .collect()
        };
        let mut parts = Vec::new();
        for p in &b.parts {
            let mut tris = Vec::new();
            for poly in p.volume_polygons() {
                let idx = polys.len() as u32;
                let area = crate::triangulate::triangulate(poly)
                    .iter()
                    .map(|t| {
                        let u = [t[1][0] - t[0][0], t[1][1] - t[0][1], t[1][2] - t[0][2]];
                        let v = [t[2][0] - t[0][0], t[2][1] - t[0][1], t[2][2] - t[0][2]];
                        let c = [
                            u[1] * v[2] - u[2] * v[1],
                            u[2] * v[0] - u[0] * v[2],
                            u[0] * v[1] - u[1] * v[0],
                        ];
                        0.5 * (c[0] * c[0] + c[1] * c[1] + c[2] * c[2]).sqrt()
                    })
                    .sum::<f64>();
                polys.push((poly.kind, area as f32));
                tris.extend(tris_of(poly, idx));
            }
            if !tris.is_empty() {
                parts.push(tris);
            }
        }
        let mut extras = Vec::new();
        for p in &b.parts {
            for poly in p.polygons.iter().filter(|q| !q.kind.bounds_volume()) {
                extras.extend(tris_of(poly, u32::MAX));
            }
        }
        let mut bb = [
            f64::INFINITY,
            f64::INFINITY,
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::NEG_INFINITY,
            f64::NEG_INFINITY,
        ];
        for t in parts.iter().flatten() {
            for v in &t.v {
                for k in 0..3 {
                    bb[k] = bb[k].min(v[k]);
                    bb[k + 3] = bb[k + 3].max(v[k]);
                }
            }
        }
        (!parts.is_empty()).then(|| Self {
            id: b.id.clone(),
            source: source.to_owned(),
            function: b.function.clone(),
            measured_height: b.measured_height,
            parts,
            bbox: bb,
            textures,
            polys,
            extras,
        })
    }

    pub fn from_building(b: &Building, source: &str) -> Option<Self> {
        Self::from_building_with(b, source, &|u| u.to_owned())
    }

    /// Merges several buildings into one whose solids are the members' parts (like
    /// BuildingParts): the voxelizer then unites them per column, so a piece standing on
    /// another neither floats nor gets a shell between the two.
    pub fn merge(id: &str, members: &[&BuildingMesh]) -> BuildingMesh {
        let mut out = BuildingMesh {
            id: id.to_owned(),
            source: members.first().map_or_else(String::new, |m| m.source.clone()),
            function: members.first().and_then(|m| m.function.clone()),
            measured_height: None,
            parts: Vec::new(),
            bbox: [
                f64::INFINITY,
                f64::INFINITY,
                f64::INFINITY,
                f64::NEG_INFINITY,
                f64::NEG_INFINITY,
                f64::NEG_INFINITY,
            ],
            textures: Vec::new(),
            polys: Vec::new(),
            extras: Vec::new(),
        };
        for m in members {
            let (tex_off, poly_off) = (out.textures.len() as u16, out.polys.len() as u32);
            let remap = |t: &MeshTri| MeshTri {
                kind: t.kind,
                v: t.v,
                poly: if t.poly == u32::MAX { u32::MAX } else { t.poly + poly_off },
                tex: t.tex.map(|(i, uv)| (i + tex_off, uv)),
                color: t.color,
            };
            out.parts.extend(m.parts.iter().map(|p| p.iter().map(remap).collect::<Vec<_>>()));
            out.extras.extend(m.extras.iter().map(remap));
            out.textures.extend(m.textures.iter().cloned());
            out.polys.extend(m.polys.iter().copied());
            for k in 0..3 {
                out.bbox[k] = out.bbox[k].min(m.bbox[k]);
                out.bbox[k + 3] = out.bbox[k + 3].max(m.bbox[k + 3]);
            }
        }
        out
    }

    /// Whether any triangle carries a texture or material color.
    pub fn has_appearance(&self) -> bool {
        self.parts.iter().flatten().any(|t| t.tex.is_some() || t.color.is_some())
    }
}
