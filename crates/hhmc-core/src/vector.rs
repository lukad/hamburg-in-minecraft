//! Reading prepared FlatGeobuf files (EPSG:25832) into plain polygons and lines.

use std::fs::File;
use std::io::BufReader;
use std::path::Path;

use anyhow::{Context, Result};
use flatgeobuf::{FallibleStreamingIterator, FgbReader};
use geo_types::Geometry;
use geozero::{FeatureProperties, ToGeo};

use crate::geom::Bbox;

/// A ring as a closed or open list of (E, N) vertices.
pub type Ring = Vec<(f64, f64)>;

/// One polygon: exterior ring first, then holes.
pub type Polygon = Vec<Ring>;

#[derive(Clone, Debug)]
pub struct Feature {
    /// Requested properties in order; `None` if absent or null.
    pub props: Vec<Option<String>>,
    pub polygons: Vec<Polygon>,
    pub lines: Vec<Vec<(f64, f64)>>,
    pub points: Vec<(f64, f64)>,
}

/// Reads all features intersecting `bbox` (all if `None`) with the given properties.
pub fn read_features(path: &Path, bbox: Option<&Bbox>, props: &[&str]) -> Result<Vec<Feature>> {
    let mut f =
        BufReader::new(File::open(path).with_context(|| format!("opening {}", path.display()))?);
    let reader = FgbReader::open(&mut f)?;
    let mut iter = match bbox {
        Some(b) => reader.select_bbox(b.min_e, b.min_n, b.max_e, b.max_n)?,
        None => reader.select_all()?,
    };
    let mut out = Vec::new();
    while let Some(feat) = iter.next()? {
        let values = props
            .iter()
            .map(|p| feat.property::<String>(p).ok().filter(|s| !s.is_empty()))
            .collect();
        let mut fe =
            Feature { props: values, polygons: Vec::new(), lines: Vec::new(), points: Vec::new() };
        let g = feat.to_geo().with_context(|| format!("geometry in {}", path.display()))?;
        collect(&g, &mut fe);
        out.push(fe);
    }
    Ok(out)
}

fn coords(ls: &geo_types::LineString<f64>) -> Vec<(f64, f64)> {
    ls.0.iter().map(|c| (c.x, c.y)).collect()
}

fn collect(g: &Geometry<f64>, fe: &mut Feature) {
    match g {
        Geometry::Polygon(p) => {
            let mut rings = vec![coords(p.exterior())];
            rings.extend(p.interiors().iter().map(coords));
            fe.polygons.push(rings);
        }
        Geometry::MultiPolygon(mp) => {
            for p in &mp.0 {
                collect(&Geometry::Polygon(p.clone()), fe);
            }
        }
        Geometry::LineString(l) => fe.lines.push(coords(l)),
        Geometry::MultiLineString(ml) => fe.lines.extend(ml.0.iter().map(coords)),
        Geometry::Point(p) => fe.points.push((p.x(), p.y())),
        Geometry::MultiPoint(mp) => fe.points.extend(mp.0.iter().map(|p| (p.x(), p.y()))),
        Geometry::GeometryCollection(gc) => {
            for g in &gc.0 {
                collect(g, fe);
            }
        }
        Geometry::Line(l) => fe.lines.push(vec![(l.start.x, l.start.y), (l.end.x, l.end.y)]),
        Geometry::Rect(r) => collect(&Geometry::Polygon(r.to_polygon()), fe),
        Geometry::Triangle(t) => collect(&Geometry::Polygon(t.to_polygon()), fe),
    }
}
