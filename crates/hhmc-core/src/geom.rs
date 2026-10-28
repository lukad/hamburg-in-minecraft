//! Minimal planar geometry in EPSG:25832 meters.

use serde::{Deserialize, Serialize};

/// Axis-aligned box in EPSG:25832, `[min_e, min_n, max_e, max_n]` when serialized.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(from = "[f64; 4]", into = "[f64; 4]")]
pub struct Bbox {
    pub min_e: f64,
    pub min_n: f64,
    pub max_e: f64,
    pub max_n: f64,
}

impl Bbox {
    pub fn new(min_e: f64, min_n: f64, max_e: f64, max_n: f64) -> Self {
        Self { min_e, min_n, max_e, max_n }
    }

    /// Smallest box containing all `points` (E, N); `None` if empty.
    pub fn from_points(points: impl IntoIterator<Item = (f64, f64)>) -> Option<Self> {
        points.into_iter().fold(None, |acc, (e, n)| {
            Some(match acc {
                None => Bbox::new(e, n, e, n),
                Some(b) => b.union(&Bbox::new(e, n, e, n)),
            })
        })
    }

    pub fn union(&self, other: &Bbox) -> Bbox {
        Bbox::new(
            self.min_e.min(other.min_e),
            self.min_n.min(other.min_n),
            self.max_e.max(other.max_e),
            self.max_n.max(other.max_n),
        )
    }

    pub fn buffered(&self, d: f64) -> Bbox {
        Bbox::new(self.min_e - d, self.min_n - d, self.max_e + d, self.max_n + d)
    }

    /// Closed-interval overlap test; boxes that only touch do not intersect.
    pub fn intersects(&self, other: &Bbox) -> bool {
        self.min_e < other.max_e
            && other.min_e < self.max_e
            && self.min_n < other.max_n
            && other.min_n < self.max_n
    }

    /// Whether the point lies inside (edges included).
    pub fn contains(&self, e: f64, n: f64) -> bool {
        e >= self.min_e && e <= self.max_e && n >= self.min_n && n <= self.max_n
    }

    pub fn width(&self) -> f64 {
        self.max_e - self.min_e
    }

    pub fn height(&self) -> f64 {
        self.max_n - self.min_n
    }
}

impl From<[f64; 4]> for Bbox {
    fn from(a: [f64; 4]) -> Self {
        Bbox::new(a[0], a[1], a[2], a[3])
    }
}

impl From<Bbox> for [f64; 4] {
    fn from(b: Bbox) -> Self {
        [b.min_e, b.min_n, b.max_e, b.max_n]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn touching_boxes_do_not_intersect() {
        let a = Bbox::new(0.0, 0.0, 10.0, 10.0);
        assert!(!a.intersects(&Bbox::new(10.0, 0.0, 20.0, 10.0)));
        assert!(a.intersects(&Bbox::new(9.0, 9.0, 20.0, 20.0)));
    }

    #[test]
    fn from_points_envelope() {
        let b = Bbox::from_points([(3.0, 1.0), (-1.0, 5.0)]).unwrap();
        assert_eq!(b, Bbox::new(-1.0, 1.0, 3.0, 5.0));
        assert!(Bbox::from_points([]).is_none());
    }
}

/// Calls `fill(col, row)` for every cell of a 1 m grid (north-west corner `min_e`,
/// `max_n`, size `w` × `h`) whose center lies inside the polygon given by `rings`
/// (exterior and holes, even-odd rule).
pub fn rasterize_polygon(
    rings: &[Vec<(f64, f64)>],
    min_e: f64,
    max_n: f64,
    w: usize,
    h: usize,
    mut fill: impl FnMut(usize, usize),
) {
    let Some(bb) = Bbox::from_points(rings.iter().flatten().copied()) else { return };
    let r0 = ((max_n - bb.max_n).floor().max(0.0)) as usize;
    let r1 = ((max_n - bb.min_n).ceil().max(0.0) as usize).min(h);
    let mut xs: Vec<f64> = Vec::new();
    for row in r0..r1 {
        let y = max_n - row as f64 - 0.5;
        xs.clear();
        for ring in rings {
            for i in 0..ring.len() {
                let (x1, y1) = ring[i];
                let (x2, y2) = ring[(i + 1) % ring.len()];
                if (y1 > y) != (y2 > y) {
                    xs.push(x1 + (y - y1) * (x2 - x1) / (y2 - y1));
                }
            }
        }
        xs.sort_by(f64::total_cmp);
        for pair in xs.as_chunks::<2>().0 {
            // Cells whose center min_e + c + 0.5 lies in [pair[0], pair[1]).
            let c0 = (pair[0] - min_e - 0.5).ceil().max(0.0) as usize;
            let c1 = ((pair[1] - min_e - 0.5).ceil().max(0.0) as usize).min(w);
            for col in c0..c1 {
                fill(col, row);
            }
        }
    }
}

#[cfg(test)]
mod raster_tests {
    use super::*;

    #[test]
    fn square_with_hole() {
        let outer = vec![(0.0, 0.0), (10.0, 0.0), (10.0, 10.0), (0.0, 10.0), (0.0, 0.0)];
        let hole = vec![(3.0, 3.0), (7.0, 3.0), (7.0, 7.0), (3.0, 7.0), (3.0, 3.0)];
        let mut n = 0;
        rasterize_polygon(std::slice::from_ref(&outer), 0.0, 10.0, 10, 10, |_, _| n += 1);
        assert_eq!(n, 100);
        let mut cells = Vec::new();
        rasterize_polygon(&[outer, hole], 0.0, 10.0, 10, 10, |c, r| cells.push((c, r)));
        assert_eq!(cells.len(), 100 - 16);
        // Cell (5, 5) has center (5.5, 4.5): inside the hole.
        assert!(!cells.contains(&(5, 5)));
        assert!(cells.contains(&(0, 0)));
    }

    #[test]
    fn clipped_to_grid() {
        let tri = vec![(-5.0, -5.0), (20.0, -5.0), (20.0, 20.0)];
        let mut n = 0;
        rasterize_polygon(&[tri], 0.0, 4.0, 4, 4, |c, r| {
            assert!(c < 4 && r < 4);
            n += 1;
        });
        // Cells with center y ≤ x: 6 below the diagonal plus 4 whose center lies on it
        // (a center exactly on a left edge counts as inside).
        assert_eq!(n, 10);
    }
}
