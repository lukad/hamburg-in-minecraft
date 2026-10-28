//! Polygon (with holes) → triangles, by projecting onto the polygon's dominant plane
//! (Newell normal) and running earcut.

use crate::Polygon;

pub type Triangle = [[f64; 3]; 3];

/// Newell's normal of a ring (not normalized).
pub fn newell(ring: &[[f64; 3]]) -> [f64; 3] {
    let mut n = [0.0; 3];
    for i in 0..ring.len() {
        let (a, b) = (ring[i], ring[(i + 1) % ring.len()]);
        n[0] += (a[1] - b[1]) * (a[2] + b[2]);
        n[1] += (a[2] - b[2]) * (a[0] + b[0]);
        n[2] += (a[0] - b[0]) * (a[1] + b[1]);
    }
    n
}

/// Triangulates a polygon; returns no triangles for degenerate input.
pub fn triangulate(p: &Polygon) -> Vec<Triangle> {
    let verts: Vec<[f64; 3]> = p.rings.iter().flatten().copied().collect();
    triangulate_indices(p).into_iter().map(|t| [verts[t[0]], verts[t[1]], verts[t[2]]]).collect()
}

/// Triangles as indices into the polygon's rings concatenated in order.
pub fn triangulate_indices(p: &Polygon) -> Vec<[usize; 3]> {
    let Some(ext) = p.rings.first() else { return Vec::new() };
    if ext.len() < 3 {
        return Vec::new();
    }
    let n = newell(ext);
    let (ax, ay) = {
        let (x, y, z) = (n[0].abs(), n[1].abs(), n[2].abs());
        // Drop the axis with the largest normal component.
        if z >= x && z >= y {
            (0, 1)
        } else if y >= x {
            (0, 2)
        } else {
            (1, 2)
        }
    };
    // Coordinates relative to the first vertex keep earcut's arithmetic well conditioned.
    let o = ext[0];
    let mut flat = Vec::new();
    let mut verts: Vec<[f64; 3]> = Vec::new();
    let mut holes = Vec::new();
    for (i, ring) in p.rings.iter().enumerate() {
        if i > 0 {
            holes.push(verts.len());
        }
        for v in ring {
            flat.push(v[ax] - o[ax]);
            flat.push(v[ay] - o[ay]);
            verts.push(*v);
        }
    }
    let _ = &verts;
    let Ok(idx) = earcutr::earcut(&flat, &holes, 2) else { return Vec::new() };
    idx.as_chunks::<3>().0.to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SurfaceKind;

    fn area(t: &Triangle) -> f64 {
        let u = [t[1][0] - t[0][0], t[1][1] - t[0][1], t[1][2] - t[0][2]];
        let v = [t[2][0] - t[0][0], t[2][1] - t[0][1], t[2][2] - t[0][2]];
        let c = [u[1] * v[2] - u[2] * v[1], u[2] * v[0] - u[0] * v[2], u[0] * v[1] - u[1] * v[0]];
        0.5 * (c[0] * c[0] + c[1] * c[1] + c[2] * c[2]).sqrt()
    }

    fn poly(rings: Vec<Vec<[f64; 3]>>) -> Polygon {
        Polygon {
            id: None,
            ring_ids: vec![None; rings.len()],
            kind: SurfaceKind::Wall,
            lod: 2,
            thematic: true,
            rings,
        }
    }

    #[test]
    fn vertical_wall_with_hole() {
        // 10 × 6 wall in the x/z plane with a 2 × 2 window hole: area 56.
        let ext = vec![[0.0, 5.0, 0.0], [10.0, 5.0, 0.0], [10.0, 5.0, 6.0], [0.0, 5.0, 6.0]];
        let hole = vec![[4.0, 5.0, 2.0], [6.0, 5.0, 2.0], [6.0, 5.0, 4.0], [4.0, 5.0, 4.0]];
        let tris = triangulate(&poly(vec![ext, hole]));
        let a: f64 = tris.iter().map(area).sum();
        assert!((a - 56.0).abs() < 1e-9, "area {a}");
    }

    #[test]
    fn utm_scale_roof() {
        let ext = vec![
            [565502.686, 5933197.988, 25.443],
            [565539.056, 5933202.619, 25.406],
            [565535.0, 5933230.0, 25.4],
        ];
        assert_eq!(triangulate(&poly(vec![ext])).len(), 1);
    }
}
