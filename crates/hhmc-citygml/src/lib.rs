//! Streaming CityGML reader for buildings and polygon triangulation.
//!
//! Elements are matched by local name, so CityGML 1.0 and 2.0 (and any namespace
//! prefixes) are read the same way. Per `bldg:Building` and nested `bldg:BuildingPart`
//! it collects polygons (`gml:Polygon` with `gml:posList` or `gml:pos` rings, exterior and
//! interior) that lie inside an `lod{1..4}Solid` or `lod{1..4}MultiSurface`, tagged with the
//! enclosing thematic surface (`RoofSurface`, `WallSurface`, `GroundSurface`, …), the
//! LoD and whether they came from a `boundedBy` surface. Solids that only reference their
//! surfaces by `xlink:href` (as in Hamburg's LoD2-DE) contribute nothing themselves.

mod mesh;
mod triangulate;

use std::io::BufRead;

use anyhow::{Context, Result, bail};
use quick_xml::events::{BytesStart, Event};
use serde::{Deserialize, Serialize};

pub use mesh::{BuildingMesh, MeshTri};
pub use triangulate::{Triangle, newell, triangulate, triangulate_indices};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SurfaceKind {
    Roof,
    Wall,
    Ground,
    Closure,
    OuterCeiling,
    OuterFloor,
    Window,
    Door,
    Installation,
    /// Geometry of a solid or multi-surface not inside a thematic surface.
    Unclassified,
}

impl SurfaceKind {
    fn from_local(name: &str) -> Option<Self> {
        Some(match name {
            "RoofSurface" => Self::Roof,
            "WallSurface" => Self::Wall,
            "GroundSurface" => Self::Ground,
            "ClosureSurface" => Self::Closure,
            "OuterCeilingSurface" => Self::OuterCeiling,
            "OuterFloorSurface" => Self::OuterFloor,
            "Window" => Self::Window,
            "Door" => Self::Door,
            "BuildingInstallation" => Self::Installation,
            _ => return None,
        })
    }

    /// Whether the surface bounds the building's volume (and so takes part in the
    /// voxelizer's inside/outside parity).
    pub fn bounds_volume(self) -> bool {
        matches!(
            self,
            Self::Roof
                | Self::Wall
                | Self::Ground
                | Self::Closure
                | Self::OuterCeiling
                | Self::OuterFloor
                | Self::Unclassified
        )
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Polygon {
    pub id: Option<String>,
    /// `gml:id` of each ring, in the order of `rings`.
    pub ring_ids: Vec<Option<String>>,
    pub kind: SurfaceKind,
    pub lod: u8,
    /// From a `boundedBy` thematic surface (as opposed to a bare solid/multi-surface).
    pub thematic: bool,
    /// Exterior ring first, then holes; closing vertex removed.
    pub rings: Vec<Vec<[f64; 3]>>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Part {
    pub id: String,
    pub polygons: Vec<Polygon>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Building {
    pub id: String,
    pub function: Option<String>,
    pub measured_height: Option<f64>,
    /// Appearances declared inside the building (`app:appearance`).
    pub textures: Vec<Texture>,
    pub materials: Vec<Material>,
    /// `parts[0]` is the building's own geometry; nested BuildingParts follow.
    pub parts: Vec<Part>,
}

/// `app:ParameterizedTexture`: an image and texture coordinates per target ring.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Texture {
    pub image_uri: String,
    /// Ring id (without `#`) → texture coordinates, one (u, v) per ring vertex as given in
    /// the file (including a closing vertex if the ring has one).
    pub rings: Vec<(String, Vec<[f32; 2]>)>,
}

/// `app:X3DMaterial`: a diffuse color for target polygons.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Material {
    pub diffuse: [f32; 3],
    /// Target polygon ids (without `#`).
    pub targets: Vec<String>,
}

impl Part {
    /// The polygons that bound this part's volume, at its highest LoD, preferring
    /// thematic (`boundedBy`) surfaces over bare solid geometry so nothing is counted twice.
    pub fn volume_polygons(&self) -> Vec<&Polygon> {
        let Some(lod) =
            self.polygons.iter().filter(|p| p.kind.bounds_volume()).map(|p| p.lod).max()
        else {
            return Vec::new();
        };
        let at_lod = |p: &&Polygon| p.lod == lod && p.kind.bounds_volume();
        let thematic: Vec<&Polygon> =
            self.polygons.iter().filter(at_lod).filter(|p| p.thematic).collect();
        if !thematic.is_empty() {
            return thematic;
        }
        self.polygons.iter().filter(at_lod).collect()
    }
}

fn id_attr(e: &BytesStart) -> Option<String> {
    e.attributes()
        .flatten()
        .find(|a| a.key.local_name().as_ref() == "id")
        .map(|a| a.value.to_string())
}

fn attr(e: &BytesStart, local: &str) -> Option<String> {
    e.attributes()
        .flatten()
        .find(|a| a.key.local_name().as_ref() == local)
        .map(|a| a.value.to_string())
}

fn dims_attr(e: &BytesStart) -> Option<usize> {
    e.attributes()
        .flatten()
        .find(|a| a.key.local_name().as_ref() == "srsDimension")
        .and_then(|a| a.value.trim().parse().ok())
}

/// LoD of geometry properties like `lod2Solid`, `lod3MultiSurface`.
fn lod_of(name: &str) -> Option<u8> {
    let rest = name.strip_prefix("lod")?;
    let (d, kind) = rest.split_at(1);
    let lod: u8 = d.parse().ok()?;
    matches!(kind, "Solid" | "MultiSurface").then_some(lod)
}

#[derive(Default)]
struct State {
    building: Option<Building>,
    texture: Option<Texture>,
    tex_ring: Option<String>,
    material: Option<Material>,
    /// Depth of open `BuildingInstallation` elements: their surfaces are installations.
    installation_depth: usize,
    /// Indices into `building.parts` of the open Building/BuildingParts.
    part_stack: Vec<usize>,
    /// Open thematic surfaces (kind, depth at which they opened).
    surface_stack: Vec<SurfaceKind>,
    /// Open geometry properties (lod).
    lod_stack: Vec<u8>,
    polygon: Option<Polygon>,
    ring: Option<Vec<[f64; 3]>>,
    ring_id: Option<String>,
    coords: Option<(String, usize)>,
    text_target: Option<&'static str>,
    text: String,
}

/// Parses all buildings from `reader`, calling `emit` once per top-level building.
pub fn parse_buildings<R: BufRead>(reader: R, mut emit: impl FnMut(Building)) -> Result<()> {
    let mut xml = quick_xml::Reader::from_reader(reader);
    xml.config_mut().trim_text(false);
    let mut buf = Vec::new();
    let mut st = State::default();
    loop {
        let ev = xml
            .read_event_into(&mut buf)
            .with_context(|| format!("XML at byte {}", xml.buffer_position()))?;
        match ev {
            Event::Start(e) => start(&mut st, &e, false)?,
            Event::Empty(e) => {
                start(&mut st, &e, true)?;
            }
            Event::End(e) => {
                let name = e.local_name();
                end(&mut st, name.as_ref(), &mut emit)?;
            }
            Event::Text(t) => {
                if st.coords.is_some() || st.text_target.is_some() {
                    st.text.push_str(t.as_ref());
                }
            }
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }
    if st.building.is_some() {
        bail!("unterminated Building element");
    }
    Ok(())
}

fn start(st: &mut State, e: &BytesStart, empty: bool) -> Result<()> {
    let name = e.local_name();
    let name = name.as_ref();
    match name {
        "Building" if st.building.is_none() => {
            let id = id_attr(e).unwrap_or_default();
            st.building = Some(Building {
                id: id.clone(),
                parts: vec![Part { id, polygons: Vec::new() }],
                ..Default::default()
            });
            st.part_stack = vec![0];
        }
        "BuildingPart" if !empty => {
            if let Some(b) = st.building.as_mut() {
                b.parts.push(Part { id: id_attr(e).unwrap_or_default(), polygons: Vec::new() });
                st.part_stack.push(b.parts.len() - 1);
            }
        }
        _ if st.building.is_none() => {}
        "function" if st.part_stack.len() == 1 && st.surface_stack.is_empty() && !empty => {
            st.text_target = Some("function");
            st.text.clear();
        }
        "measuredHeight" if st.part_stack.len() == 1 && !empty => {
            st.text_target = Some("measuredHeight");
            st.text.clear();
        }
        "ParameterizedTexture" if !empty => st.texture = Some(Texture::default()),
        "imageURI" if !empty && st.texture.is_some() => {
            st.text_target = Some("imageURI");
            st.text.clear();
        }
        "textureCoordinates" if !empty && st.texture.is_some() => {
            st.tex_ring = attr(e, "ring").map(|r| r.trim_start_matches('#').to_owned());
            st.text_target = Some("textureCoordinates");
            st.text.clear();
        }
        "X3DMaterial" if !empty => st.material = Some(Material::default()),
        "diffuseColor" if !empty && st.material.is_some() => {
            st.text_target = Some("diffuseColor");
            st.text.clear();
        }
        "target" if !empty && st.material.is_some() => {
            st.text_target = Some("materialTarget");
            st.text.clear();
        }
        _ if SurfaceKind::from_local(name).is_some() && !empty => {
            let kind = SurfaceKind::from_local(name).expect("checked");
            if kind == SurfaceKind::Installation {
                st.installation_depth += 1;
            }
            st.surface_stack.push(kind);
        }
        _ if lod_of(name).is_some() && !empty => st.lod_stack.push(lod_of(name).expect("checked")),
        "Polygon" if !empty && !st.lod_stack.is_empty() => {
            st.polygon = Some(Polygon {
                id: id_attr(e),
                ring_ids: Vec::new(),
                kind: if st.installation_depth > 0 {
                    SurfaceKind::Installation
                } else {
                    st.surface_stack.last().copied().unwrap_or(SurfaceKind::Unclassified)
                },
                lod: *st.lod_stack.last().expect("non-empty"),
                thematic: !st.surface_stack.is_empty(),
                rings: Vec::new(),
            });
        }
        "LinearRing" if !empty && st.polygon.is_some() => {
            st.ring = Some(Vec::new());
            st.ring_id = id_attr(e);
        }
        "posList" | "pos" if !empty && st.ring.is_some() => {
            st.coords = Some((name.to_owned(), dims_attr(e).unwrap_or(3)));
            st.text.clear();
        }
        _ => {}
    }
    Ok(())
}

fn end(st: &mut State, name: &str, emit: &mut impl FnMut(Building)) -> Result<()> {
    if st.building.is_none() {
        return Ok(());
    }
    match name {
        "posList" | "pos" if st.coords.is_some() => {
            let (_, dims) = st.coords.take().expect("checked");
            let vals: Vec<f64> = st
                .text
                .split_ascii_whitespace()
                .map(str::parse)
                .collect::<Result<_, _>>()
                .context("bad coordinate")?;
            if dims != 3 || !vals.len().is_multiple_of(3) {
                bail!(
                    "expected 3D coordinates, got srsDimension {dims} with {} values",
                    vals.len()
                );
            }
            if let Some(r) = st.ring.as_mut() {
                r.extend(vals.as_chunks::<3>().0.iter().copied());
            }
            st.text.clear();
        }
        "LinearRing" => {
            if let (Some(mut ring), Some(p)) = (st.ring.take(), st.polygon.as_mut()) {
                if ring.len() > 1 && ring.first() == ring.last() {
                    ring.pop();
                }
                if ring.len() >= 3 {
                    p.rings.push(ring);
                    p.ring_ids.push(st.ring_id.take());
                }
            }
        }
        "Polygon" => {
            if let (Some(p), Some(b)) = (st.polygon.take(), st.building.as_mut())
                && !p.rings.is_empty()
            {
                let part = *st.part_stack.last().expect("inside building");
                b.parts[part].polygons.push(p);
            }
        }
        "function" | "measuredHeight" | "imageURI" | "textureCoordinates" | "diffuseColor"
        | "target"
            if st.text_target.is_some() =>
        {
            let b = st.building.as_mut().expect("checked");
            let text = st.text.trim();
            match st.text_target.take() {
                Some("function") => b.function = Some(text.to_owned()),
                Some("measuredHeight") => b.measured_height = text.parse().ok(),
                Some("imageURI") => {
                    if let Some(t) = st.texture.as_mut() {
                        t.image_uri = text.to_owned();
                    }
                }
                Some("textureCoordinates") => {
                    let v: Vec<f32> =
                        text.split_ascii_whitespace().filter_map(|x| x.parse().ok()).collect();
                    if let (Some(t), Some(ring)) = (st.texture.as_mut(), st.tex_ring.take()) {
                        t.rings.push((ring, v.as_chunks::<2>().0.to_vec()));
                    }
                }
                Some("diffuseColor") => {
                    let v: Vec<f32> =
                        text.split_ascii_whitespace().filter_map(|x| x.parse().ok()).collect();
                    if let (Some(m), [r, g, bl]) = (st.material.as_mut(), v.as_slice()) {
                        m.diffuse = [*r, *g, *bl];
                    }
                }
                Some("materialTarget") => {
                    if let Some(m) = st.material.as_mut() {
                        m.targets.push(text.trim_start_matches('#').to_owned());
                    }
                }
                _ => {}
            }
            st.text.clear();
        }
        "ParameterizedTexture" => {
            if let (Some(t), Some(b)) = (st.texture.take(), st.building.as_mut()) {
                b.textures.push(t);
            }
        }
        "X3DMaterial" => {
            if let (Some(m), Some(b)) = (st.material.take(), st.building.as_mut()) {
                b.materials.push(m);
            }
        }
        _ if SurfaceKind::from_local(name).is_some() => {
            if st.surface_stack.pop() == Some(SurfaceKind::Installation) {
                st.installation_depth -= 1;
            }
        }
        _ if lod_of(name).is_some() => {
            st.lod_stack.pop();
        }
        "BuildingPart" => {
            st.part_stack.pop();
        }
        "Building" => {
            let b = st.building.take().expect("checked");
            st.part_stack.clear();
            st.surface_stack.clear();
            st.lod_stack.clear();
            st.installation_depth = 0;
            emit(b);
        }
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const V2_PART: &str = r#"<?xml version="1.0"?>
<core:CityModel xmlns:core="http://www.opengis.net/citygml/2.0" xmlns:bldg="http://www.opengis.net/citygml/building/2.0" xmlns:gml="http://www.opengis.net/gml">
 <core:cityObjectMember><bldg:Building gml:id="B1">
  <bldg:function>31001_1010</bldg:function>
  <bldg:measuredHeight uom="m">6.5</bldg:measuredHeight>
  <bldg:boundedBy><bldg:RoofSurface gml:id="R"><bldg:lod2MultiSurface><gml:MultiSurface><gml:surfaceMember>
   <gml:Polygon gml:id="P1"><gml:exterior><gml:LinearRing>
    <gml:pos>0 0 6</gml:pos><gml:pos>4 0 6</gml:pos><gml:pos>4 4 6</gml:pos><gml:pos>0 0 6</gml:pos>
   </gml:LinearRing></gml:exterior></gml:Polygon>
  </gml:surfaceMember></gml:MultiSurface></bldg:lod2MultiSurface></bldg:RoofSurface></bldg:boundedBy>
  <bldg:consistsOfBuildingPart><bldg:BuildingPart gml:id="BP1">
   <bldg:lod2Solid><gml:Solid><gml:exterior><gml:CompositeSurface><gml:surfaceMember>
    <gml:Polygon><gml:exterior><gml:LinearRing><gml:posList srsDimension="3">0 0 0 1 0 0 1 1 0 0 0 0</gml:posList></gml:LinearRing></gml:exterior>
    <gml:interior><gml:LinearRing><gml:posList>0.2 0.1 0 0.3 0.1 0 0.3 0.2 0</gml:posList></gml:LinearRing></gml:interior></gml:Polygon>
   </gml:surfaceMember></gml:CompositeSurface></gml:exterior></gml:Solid></bldg:lod2Solid>
  </bldg:BuildingPart></bldg:consistsOfBuildingPart>
 </bldg:Building></core:cityObjectMember>
</core:CityModel>"#;

    #[test]
    fn parses_citygml2_with_part_pos_and_holes() {
        let mut out = Vec::new();
        parse_buildings(V2_PART.as_bytes(), |b| out.push(b)).unwrap();
        assert_eq!(out.len(), 1);
        let b = &out[0];
        assert_eq!(b.id, "B1");
        assert_eq!(b.function.as_deref(), Some("31001_1010"));
        assert_eq!(b.measured_height, Some(6.5));
        assert_eq!(b.parts.len(), 2);
        let roof = &b.parts[0].polygons[0];
        assert_eq!((roof.kind, roof.lod, roof.thematic), (SurfaceKind::Roof, 2, true));
        assert_eq!(roof.rings[0].len(), 3, "closing vertex dropped");
        let p = &b.parts[1];
        assert_eq!(p.id, "BP1");
        assert_eq!(p.polygons[0].kind, SurfaceKind::Unclassified);
        assert_eq!(p.polygons[0].rings.len(), 2);
    }

    #[test]
    fn appearance_texture_coordinates_reach_triangles() {
        let xml = r##"<CityModel><cityObjectMember><Building id="B">
          <appearance><Appearance><surfaceDataMember><ParameterizedTexture>
            <imageURI>images\a.jpg</imageURI>
            <target uri="#P"><TexCoordList><textureCoordinates ring="#R">0 0 1 0 1 1 0 0</textureCoordinates></TexCoordList></target>
          </ParameterizedTexture></surfaceDataMember>
          <surfaceDataMember><X3DMaterial><diffuseColor>1 0.5 0</diffuseColor><target>#Q</target></X3DMaterial></surfaceDataMember>
          </Appearance></appearance>
          <boundedBy><WallSurface><lod3MultiSurface><MultiSurface><surfaceMember>
            <Polygon id="P"><exterior><LinearRing id="R"><posList>0 0 0 1 0 0 1 0 1 0 0 0</posList></LinearRing></exterior></Polygon>
          </surfaceMember><surfaceMember>
            <Polygon id="Q"><exterior><LinearRing id="S"><posList>0 1 0 1 1 0 1 1 1 0 1 0</posList></LinearRing></exterior></Polygon>
          </surfaceMember></MultiSurface></lod3MultiSurface></WallSurface></boundedBy>
          <outerBuildingInstallation><BuildingInstallation><boundedBy><RoofSurface><lod3MultiSurface><MultiSurface><surfaceMember>
            <Polygon id="I"><exterior><LinearRing><posList>5 5 5 6 5 5 6 6 5 5 5 5</posList></LinearRing></exterior></Polygon>
          </surfaceMember></MultiSurface></lod3MultiSurface></RoofSurface></boundedBy></BuildingInstallation></outerBuildingInstallation>
        </Building></cityObjectMember></CityModel>"##;
        let mut out = Vec::new();
        parse_buildings(xml.as_bytes(), |b| out.push(b)).unwrap();
        let b = &out[0];
        assert_eq!(b.textures[0].image_uri, "images\\a.jpg");
        assert_eq!(b.textures[0].rings[0].0, "R");
        assert_eq!(b.materials[0].diffuse, [1.0, 0.5, 0.0]);
        let kinds: Vec<_> = b.parts[0].polygons.iter().map(|p| p.kind).collect();
        assert_eq!(kinds, vec![SurfaceKind::Wall, SurfaceKind::Wall, SurfaceKind::Installation]);
        let m = BuildingMesh::from_building(b, "t").unwrap();
        let tri = &m.parts[0][0];
        let (ti, uv) = tri.tex.unwrap();
        assert_eq!(ti, 0);
        // The triangle's vertices map to the texture coordinates of the same ring vertices.
        for (v, t) in tri.v.iter().zip(uv) {
            assert_eq!([v[0] as f32, v[2] as f32], t);
        }
        assert_eq!(m.parts[0][1].color, Some([255, 128, 0]));
        assert_eq!(m.extras.len(), 1);
    }

    #[test]
    fn xlink_only_solid_adds_nothing() {
        let xml = r##"<CityModel><cityObjectMember><Building id="X"><lod2Solid><Solid><exterior><CompositeSurface>
            <surfaceMember href="#p"/></CompositeSurface></exterior></Solid></lod2Solid></Building></cityObjectMember></CityModel>"##;
        let mut out = Vec::new();
        parse_buildings(xml.as_bytes(), |b| out.push(b)).unwrap();
        assert!(out[0].parts[0].polygons.is_empty());
    }
}
