//! Dumps NAS objects of the given classes as GeoJSON (EPSG:25832), for inspection:
//! `cargo run --example nas2geojson -- <out.geojson> <class,class,…> <nas.xml>...`
use hhmc_prep::nas::{NasGeom, read};

fn main() -> anyhow::Result<()> {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let classes: Vec<&str> = a[1].split(',').collect();
    let feats = read(&a[2..], &classes)?;
    let ring = |r: &Vec<(f64, f64)>| r.iter().map(|p| vec![p.0, p.1]).collect::<Vec<_>>();
    let fs: Vec<serde_json::Value> = feats
        .iter()
        .map(|f| {
            let geom = match &f.geom {
                NasGeom::Polygons(p) => serde_json::json!({"type": "MultiPolygon",
                    "coordinates": p.iter().map(|rs| rs.iter().map(ring).collect::<Vec<_>>()).collect::<Vec<_>>()}),
                NasGeom::Lines(l) => serde_json::json!({"type": "MultiLineString",
                    "coordinates": l.iter().map(ring).collect::<Vec<_>>()}),
                NasGeom::Point(p) => serde_json::json!({"type": "Point", "coordinates": [p.0, p.1]}),
                NasGeom::None => serde_json::Value::Null,
            };
            let mut props = serde_json::Map::new();
            props.insert("class".into(), f.class.clone().into());
            props.insert("id".into(), f.id.clone().into());
            for (k, v) in &f.props {
                props.insert(k.clone(), v.clone().into());
            }
            serde_json::json!({"type": "Feature", "properties": props, "geometry": geom})
        })
        .collect();
    std::fs::write(
        &a[0],
        serde_json::to_string(&serde_json::json!({"type": "FeatureCollection", "features": fs}))?,
    )?;
    eprintln!("{} features", feats.len());
    Ok(())
}
