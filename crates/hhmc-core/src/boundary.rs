//! The city boundary that a boundary-defined area is clipped to: a
//! GeoJSON (Multi)Polygon in EPSG:25832, of which only parts of at least a minimum area
//! are kept (the mainland without Neuwerk and Scharhörn). A column is inside if its
//! center is (even-odd over all rings), the same rule as every other polygon raster.

use std::path::Path;

use anyhow::{Context, Result, bail};
use serde_json::Value;

use crate::geom::{Bbox, rasterize_polygon};
use crate::vector::Polygon;

#[derive(Clone, Debug)]
pub struct Boundary {
    pub polygons: Vec<Polygon>,
}

/// Area of a ring by the shoelace formula, unsigned.
fn ring_area(r: &[(f64, f64)]) -> f64 {
    let mut a = 0.0;
    for i in 0..r.len() {
        let ((x1, y1), (x2, y2)) = (r[i], r[(i + 1) % r.len()]);
        a += x1 * y2 - x2 * y1;
    }
    a.abs() / 2.0
}

fn polygon_area(p: &Polygon) -> f64 {
    p.first().map_or(0.0, |e| ring_area(e)) - p.iter().skip(1).map(|h| ring_area(h)).sum::<f64>()
}

fn parse_polygon(v: &Value) -> Result<Polygon> {
    let rings = v.as_array().context("polygon is not an array")?;
    rings
        .iter()
        .map(|r| {
            r.as_array()
                .context("ring is not an array")?
                .iter()
                .map(|c| {
                    let x = c.get(0).and_then(Value::as_f64).context("bad coordinate")?;
                    let y = c.get(1).and_then(Value::as_f64).context("bad coordinate")?;
                    Ok((x, y))
                })
                .collect()
        })
        .collect()
}

impl Boundary {
    /// Reads every Polygon and MultiPolygon in a GeoJSON FeatureCollection (EPSG:25832)
    /// and keeps the parts of at least `min_part_km2`.
    pub fn load(path: &Path, min_part_km2: f64) -> Result<Self> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let doc: Value = serde_json::from_str(&text)?;
        let mut all = Vec::new();
        for f in doc["features"].as_array().context("no features")? {
            let g = &f["geometry"];
            match g["type"].as_str() {
                Some("Polygon") => all.push(parse_polygon(&g["coordinates"])?),
                Some("MultiPolygon") => {
                    for p in g["coordinates"].as_array().context("bad MultiPolygon")? {
                        all.push(parse_polygon(p)?);
                    }
                }
                _ => {}
            }
        }
        let polygons: Vec<Polygon> =
            all.into_iter().filter(|p| polygon_area(p) >= min_part_km2 * 1e6).collect();
        if polygons.is_empty() {
            bail!("{}: no polygon part of at least {min_part_km2} km²", path.display());
        }
        Ok(Self { polygons })
    }

    pub fn area_km2(&self) -> f64 {
        self.polygons.iter().map(polygon_area).sum::<f64>() / 1e6
    }

    pub fn bbox(&self) -> Bbox {
        Bbox::from_points(self.polygons.iter().flat_map(|p| p[0].iter().copied()))
            .expect("non-empty boundary")
    }

    /// Inside flags of a 1 m grid (north-west corner `min_e`, `max_n`, size `w` × `h`),
    /// row-major.
    pub fn mask(&self, min_e: f64, max_n: f64, w: usize, h: usize) -> Vec<bool> {
        let mut m = vec![false; w * h];
        let grid = Bbox::new(min_e, max_n - h as f64, min_e + w as f64, max_n);
        for p in &self.polygons {
            if Bbox::from_points(p[0].iter().copied()).is_some_and(|b| b.intersects(&grid)) {
                rasterize_polygon(p, min_e, max_n, w, h, |c, r| m[r * w + c] = true);
            }
        }
        m
    }

    /// Calls `cell(col, row)` for every inside column of the 1 m grid covering `bbox`
    /// (snapped outward to whole meters); for walking a whole city without a full mask.
    pub fn for_each_inside(&self, bbox: &Bbox, mut cell: impl FnMut(usize, usize)) {
        let (min_e, max_n) = (bbox.min_e.floor(), bbox.max_n.ceil());
        let w = (bbox.max_e.ceil() - min_e) as usize;
        let h = (max_n - bbox.min_n.floor()) as usize;
        for p in &self.polygons {
            rasterize_polygon(p, min_e, max_n, w, h, &mut cell);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_parts_are_dropped_and_holes_stay_outside() {
        let dir = std::env::temp_dir().join(format!("hhmc-boundary-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("b.geojson");
        // A 4 × 4 km square with a 1 km hole, and a 0.5 × 0.5 km island.
        std::fs::write(
            &p,
            r#"{"type":"FeatureCollection","features":[{"type":"Feature","properties":{},
            "geometry":{"type":"MultiPolygon","coordinates":[
              [[[0,0],[4000,0],[4000,4000],[0,4000],[0,0]],
               [[1000,1000],[2000,1000],[2000,2000],[1000,2000],[1000,1000]]],
              [[[10000,0],[10500,0],[10500,500],[10000,500],[10000,0]]]]}}]}"#,
        )
        .unwrap();
        let b = Boundary::load(&p, 1.0).unwrap();
        assert_eq!(b.polygons.len(), 1);
        assert!((b.area_km2() - 15.0).abs() < 1e-9);
        let m = b.mask(1990.0, 2010.0, 20, 20);
        // Row 0 is n 2009..2010 (outside the hole's top at 2000), column 0 e 1990..1991
        // (inside the hole's x range): inside the square.
        assert!(m[0]);
        // Row 15 (n 1994..1995), column 5 (e 1995..1996): in the hole.
        assert!(!m[15 * 20 + 5]);
        // Column 15 (e 2005): right of the hole, inside.
        assert!(m[15 * 20 + 15]);
        assert_eq!(Boundary::load(&p, 0.1).unwrap().polygons.len(), 2);
    }
}
