//! A small streaming reader for ALKIS NAS (GML 3.2) objects of selected classes: the
//! object's simple properties and its `position` geometry, with arcs and circles
//! densified to line segments.
//!
//! GDAL's NAS driver infers its schema from the first file it reads and drops attributes
//! that file lacks (`name`, `bauwerksfunktion` of later parts), so the few structure
//! classes the infrastructure stage needs are read here.

use std::collections::{BTreeMap, HashSet};
use std::io::BufReader;
use std::path::Path;

use anyhow::{Context, Result};
use quick_xml::events::Event;
use serde::{Deserialize, Serialize};

/// Densified arcs have segments of at most this length (m).
const ARC_STEP_M: f64 = 0.5;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum NasGeom {
    /// Polygons, each as exterior ring followed by its holes (closed rings).
    Polygons(Vec<Vec<Vec<(f64, f64)>>>),
    /// Lines (curves; a composite curve is one line).
    Lines(Vec<Vec<(f64, f64)>>),
    Point((f64, f64)),
    None,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NasFeature {
    /// Object class, e.g. `AX_BauwerkImVerkehrsbereich`.
    pub class: String,
    /// `gml:id`.
    pub id: String,
    /// Simple (text-only) child properties of the object, e.g. `bauwerksfunktion`.
    pub props: BTreeMap<String, String>,
    pub geom: NasGeom,
}

impl NasFeature {
    pub fn prop(&self, k: &str) -> Option<&str> {
        self.props.get(k).map(String::as_str)
    }

    pub fn int(&self, k: &str) -> Option<i64> {
        self.prop(k).and_then(|v| v.trim().parse().ok())
    }
}

#[derive(Default)]
struct Geo {
    polygons: Vec<Vec<Vec<(f64, f64)>>>,
    lines: Vec<Vec<(f64, f64)>>,
    point: Option<(f64, f64)>,
    /// Ring or line being assembled from segments.
    cur: Vec<(f64, f64)>,
    in_polygon: bool,
    rings: Vec<Vec<(f64, f64)>>,
    /// Segment kind of the current `posList` / `pos`.
    seg: Seg,
    curve_depth: usize,
}

#[derive(Clone, Copy, Default, PartialEq)]
enum Seg {
    #[default]
    Line,
    Arc,
    Circle,
}

/// Reads the objects of `classes` from NAS files; objects repeated in several files
/// (overlapping deliveries) are kept once, by `gml:id`.
pub fn read(files: &[impl AsRef<Path>], classes: &[&str]) -> Result<Vec<NasFeature>> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for f in files {
        for feat in read_file(f.as_ref(), classes)? {
            if seen.insert(feat.id.clone()) {
                out.push(feat);
            }
        }
    }
    out.sort_by(|a, b| (&a.class, &a.id).cmp(&(&b.class, &b.id)));
    Ok(out)
}

fn read_file(path: &Path, classes: &[&str]) -> Result<Vec<NasFeature>> {
    let mut r = quick_xml::Reader::from_reader(BufReader::new(
        std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?,
    ));
    let mut buf = Vec::new();
    let mut out = Vec::new();
    // Current object: (feature, depth of its element).
    let mut obj: Option<(NasFeature, usize)> = None;
    let mut geo = Geo::default();
    let mut depth = 0usize;
    let mut in_position = false;
    let mut text = String::new();
    let mut text_open = false;
    loop {
        let ev = r.read_event_into(&mut buf).with_context(|| format!("NAS {}", path.display()))?;
        match ev {
            Event::Start(e) => {
                depth += 1;
                let name = e.local_name();
                let name = name.as_ref();
                match &mut obj {
                    None => {
                        if classes.contains(&name) {
                            let id = e
                                .attributes()
                                .flatten()
                                .find(|a| a.key.local_name().as_ref() == "id")
                                .map(|a| a.value.to_string())
                                .unwrap_or_default();
                            obj = Some((
                                NasFeature {
                                    class: name.to_string(),
                                    id,
                                    props: BTreeMap::new(),
                                    geom: NasGeom::None,
                                },
                                depth,
                            ));
                            geo = Geo::default();
                        }
                    }
                    Some((_, od)) => {
                        let od = *od;
                        if depth == od + 1 && name == "position" {
                            in_position = true;
                        }
                        if in_position {
                            match name {
                                "PolygonPatch" | "Polygon" => {
                                    geo.in_polygon = true;
                                    geo.rings.clear();
                                }
                                "exterior" | "interior" => geo.cur.clear(),
                                "Curve" | "CompositeCurve" | "LineString" => {
                                    if !geo.in_polygon && geo.curve_depth == 0 {
                                        geo.cur.clear();
                                    }
                                    geo.curve_depth += 1;
                                }
                                "LineStringSegment" => geo.seg = Seg::Line,
                                "Arc" | "ArcString" => geo.seg = Seg::Arc,
                                "Circle" => geo.seg = Seg::Circle,
                                "posList" | "pos" => {
                                    text.clear();
                                    text_open = true;
                                }
                                _ => {}
                            }
                        } else if depth == od + 1 {
                            text.clear();
                            text_open = true;
                        } else {
                            // Nested structure (not a simple property).
                            text_open = false;
                        }
                    }
                }
            }
            Event::Empty(_) => {}
            Event::Text(t) => {
                if text_open {
                    text.push_str(t.as_ref());
                }
            }
            Event::End(e) => {
                let name = e.local_name();
                let name = name.as_ref();
                if let Some((feat, od)) = &mut obj {
                    let od = *od;
                    if depth == od {
                        feat.geom = if !geo.polygons.is_empty() {
                            NasGeom::Polygons(std::mem::take(&mut geo.polygons))
                        } else if !geo.lines.is_empty() {
                            NasGeom::Lines(std::mem::take(&mut geo.lines))
                        } else if let Some(p) = geo.point {
                            NasGeom::Point(p)
                        } else {
                            NasGeom::None
                        };
                        out.push(obj.take().expect("object").0);
                    } else if in_position {
                        match name {
                            "posList" | "pos" if text_open => {
                                text_open = false;
                                let v: Vec<f64> = text
                                    .split_whitespace()
                                    .filter_map(|s| s.parse().ok())
                                    .collect();
                                let pts: Vec<(f64, f64)> =
                                    v.as_chunks::<2>().0.iter().map(|c| (c[0], c[1])).collect();
                                if name == "pos" && !geo.in_polygon && geo.curve_depth == 0 {
                                    geo.point = pts.first().copied();
                                } else {
                                    let pts = match geo.seg {
                                        Seg::Line => pts,
                                        Seg::Arc => densify_arcs(&pts),
                                        Seg::Circle => densify_circle(&pts),
                                    };
                                    append(&mut geo.cur, &pts);
                                }
                            }
                            "exterior" | "interior" => {
                                let mut ring = std::mem::take(&mut geo.cur);
                                if ring.len() >= 3 {
                                    if ring.first() != ring.last() {
                                        ring.push(ring[0]);
                                    }
                                    if name == "exterior" {
                                        geo.rings.insert(0, ring);
                                    } else {
                                        geo.rings.push(ring);
                                    }
                                }
                            }
                            "PolygonPatch" | "Polygon" => {
                                geo.in_polygon = false;
                                if !geo.rings.is_empty() {
                                    geo.polygons.push(std::mem::take(&mut geo.rings));
                                }
                            }
                            "Curve" | "CompositeCurve" | "LineString" => {
                                geo.curve_depth = geo.curve_depth.saturating_sub(1);
                                if !geo.in_polygon && geo.curve_depth == 0 && geo.cur.len() >= 2 {
                                    geo.lines.push(std::mem::take(&mut geo.cur));
                                }
                            }
                            "position" if depth == od + 1 => in_position = false,
                            _ => {}
                        }
                    } else if depth == od + 1 && text_open {
                        let v = text.trim();
                        if !v.is_empty() {
                            feat.props.insert(name.to_string(), v.to_string());
                        }
                        text_open = false;
                    }
                }
                depth -= 1;
            }
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }
    Ok(out)
}

/// Appends points, skipping the first if it repeats the last one (segment joints).
fn append(cur: &mut Vec<(f64, f64)>, pts: &[(f64, f64)]) {
    for (i, &p) in pts.iter().enumerate() {
        if i == 0 && cur.last().is_some_and(|q| (q.0 - p.0).hypot(q.1 - p.1) < 1e-6) {
            continue;
        }
        cur.push(p);
    }
}

/// Center of the circle through three points, if they are not collinear.
fn circle_center(a: (f64, f64), b: (f64, f64), c: (f64, f64)) -> Option<(f64, f64)> {
    let d = 2.0 * (a.0 * (b.1 - c.1) + b.0 * (c.1 - a.1) + c.0 * (a.1 - b.1));
    if d.abs() < 1e-9 {
        return None;
    }
    let (a2, b2, c2) = (a.0 * a.0 + a.1 * a.1, b.0 * b.0 + b.1 * b.1, c.0 * c.0 + c.1 * c.1);
    Some((
        (a2 * (b.1 - c.1) + b2 * (c.1 - a.1) + c2 * (a.1 - b.1)) / d,
        (a2 * (c.0 - b.0) + b2 * (a.0 - c.0) + c2 * (b.0 - a.0)) / d,
    ))
}

/// Points along arcs given as start, middle, end (an arc string chains them).
fn densify_arcs(p: &[(f64, f64)]) -> Vec<(f64, f64)> {
    let mut out = vec![];
    if p.is_empty() {
        return out;
    }
    out.push(p[0]);
    let mut i = 0;
    while i + 2 < p.len() {
        let (a, b, c) = (p[i], p[i + 1], p[i + 2]);
        match circle_center(a, b, c) {
            None => out.extend([b, c]),
            Some(o) => {
                let r = (a.0 - o.0).hypot(a.1 - o.1);
                let ang = |q: (f64, f64)| (q.1 - o.1).atan2(q.0 - o.0);
                let (t0, t1, t2) = (ang(a), ang(b), ang(c));
                // Sweep from a to c through b: counter-clockwise if b lies on that way.
                let ccw = |from: f64, to: f64| (to - from).rem_euclid(std::f64::consts::TAU);
                let sweep = if ccw(t0, t1) <= ccw(t0, t2) { ccw(t0, t2) } else { -ccw(t2, t0) };
                let n = ((sweep.abs() * r / ARC_STEP_M).ceil() as usize).max(2);
                for k in 1..n {
                    let t = t0 + sweep * k as f64 / n as f64;
                    out.push((o.0 + r * t.cos(), o.1 + r * t.sin()));
                }
                out.push(c);
            }
        }
        i += 2;
    }
    out
}

/// A full circle through three points, closed.
fn densify_circle(p: &[(f64, f64)]) -> Vec<(f64, f64)> {
    let [a, b, c] = match p {
        [a, b, c, ..] => [*a, *b, *c],
        _ => return p.to_vec(),
    };
    let Some(o) = circle_center(a, b, c) else { return p.to_vec() };
    let r = (a.0 - o.0).hypot(a.1 - o.1);
    let t0 = (a.1 - o.1).atan2(a.0 - o.0);
    let n = ((std::f64::consts::TAU * r / ARC_STEP_M).ceil() as usize).max(8);
    let mut out: Vec<(f64, f64)> = (0..n)
        .map(|k| {
            let t = t0 + std::f64::consts::TAU * k as f64 / n as f64;
            (o.0 + r * t.cos(), o.1 + r * t.sin())
        })
        .collect();
    out.push(out[0]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const DOC: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<AX_Bestandsdatenauszug xmlns:gml="http://www.opengis.net/gml/3.2"><enthaelt><wfs:FeatureCollection xmlns:wfs="x"><wfs:member>
<AX_BauwerkImVerkehrsbereich gml:id="B1">
  <lebenszeitintervall><AA_Lebenszeitintervall><beginnt>2020</beginnt></AA_Lebenszeitintervall></lebenszeitintervall>
  <position><gml:Surface gml:id="B1_G0"><gml:patches><gml:PolygonPatch><gml:exterior><gml:Ring>
    <gml:curveMember><gml:Curve><gml:segments><gml:LineStringSegment><gml:posList>0 0 10 0 10 10</gml:posList></gml:LineStringSegment></gml:segments></gml:Curve></gml:curveMember>
    <gml:curveMember><gml:Curve><gml:segments><gml:Arc><gml:posList>10 10 5 15 0 10</gml:posList></gml:Arc></gml:segments></gml:Curve></gml:curveMember>
    <gml:curveMember><gml:Curve><gml:segments><gml:LineStringSegment><gml:posList>0 10 0 0</gml:posList></gml:LineStringSegment></gml:segments></gml:Curve></gml:curveMember>
  </gml:Ring></gml:exterior></gml:PolygonPatch></gml:patches></gml:Surface></position>
  <bauwerksfunktion>1800</bauwerksfunktion>
  <name>Kornhausbrücke</name>
</AX_BauwerkImVerkehrsbereich>
</wfs:member><wfs:member>
<AX_Gleis gml:id="G1"><position><gml:Curve><gml:segments><gml:LineStringSegment><gml:posList>0 0 5 0</gml:posList></gml:LineStringSegment><gml:LineStringSegment><gml:posList>5 0 5 5</gml:posList></gml:LineStringSegment></gml:segments></gml:Curve></position><bahnkategorie>1100</bahnkategorie></AX_Gleis>
</wfs:member><wfs:member>
<AX_Gebaeude gml:id="X"><position/></AX_Gebaeude>
</wfs:member></wfs:FeatureCollection></enthaelt></AX_Bestandsdatenauszug>"#;

    #[test]
    fn reads_polygons_arcs_lines_and_props() {
        let dir = std::env::temp_dir().join(format!("hhmc-nas-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("a.xml");
        std::fs::write(&f, DOC).unwrap();
        let v = read(&[&f, &f], &["AX_BauwerkImVerkehrsbereich", "AX_Gleis"]).unwrap();
        assert_eq!(v.len(), 2);
        let b = &v[0];
        assert_eq!((b.class.as_str(), b.id.as_str()), ("AX_BauwerkImVerkehrsbereich", "B1"));
        assert_eq!(b.int("bauwerksfunktion"), Some(1800));
        assert_eq!(b.prop("name"), Some("Kornhausbrücke"));
        assert!(b.prop("beginnt").is_none());
        let NasGeom::Polygons(p) = &b.geom else { panic!("{:?}", b.geom) };
        let ring = &p[0][0];
        assert_eq!(ring.first(), ring.last());
        // The arc bulges to y = 15 (radius 5 around (5, 10)).
        let top = ring.iter().map(|q| q.1).fold(f64::MIN, f64::max);
        assert!((top - 15.0).abs() < 0.05, "{top}");
        assert!(
            ring.iter().all(|q| q.1 <= 10.0 || ((q.0 - 5.0).hypot(q.1 - 10.0) - 5.0).abs() < 1e-6)
        );
        let g = &v[1];
        assert_eq!(g.geom, NasGeom::Lines(vec![vec![(0.0, 0.0), (5.0, 0.0), (5.0, 5.0)]]));
        std::fs::remove_dir_all(&dir).ok();
    }
}
