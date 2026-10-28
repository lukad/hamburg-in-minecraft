//! Named tile sets from `config/areas.toml`.

use std::collections::BTreeMap;

use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};

use crate::boundary::Boundary;
use crate::config::{Origin, Paths, load_toml};
use crate::geom::Bbox;
use crate::tile::{TILE_BUFFER, TileKey};

/// `config/areas.toml`: area name → definition.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Areas(pub BTreeMap<String, Area>);

impl Areas {
    pub fn load(paths: &Paths) -> Result<Self> {
        load_toml(&paths.config().join("areas.toml"))
    }

    pub fn get(&self, name: &str) -> Result<&Area> {
        self.0.get(name).ok_or_else(|| {
            let known: Vec<_> = self.0.keys().cloned().collect();
            anyhow!("unknown area `{name}`; known: {}", known.join(", "))
        })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Area {
    pub description: String,
    /// South-west corner the area was derived from, `[lat, lon]` in WGS84.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wgs84_sw: Option<[f64; 2]>,
    /// North-east corner, `[lat, lon]` in WGS84.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wgs84_ne: Option<[f64; 2]>,
    /// Envelope of the transformed WGS84 box in EPSG:25832, before snapping.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bbox_25832: Option<Bbox>,
    /// Clip to a city boundary: columns outside it stay empty.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub boundary: Option<AreaBoundary>,
    pub tiles: Vec<TileKey>,
}

/// The boundary an area is clipped to.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AreaBoundary {
    /// GeoJSON in EPSG:25832, relative to the repository root.
    pub path: String,
    /// Polygon parts smaller than this are left out (islands).
    pub min_part_km2: f64,
}

impl Area {
    /// The boundary this area is clipped to, if any.
    pub fn boundary(&self, paths: &Paths) -> Result<Option<Boundary>> {
        self.boundary
            .as_ref()
            .map(|b| Boundary::load(&paths.root.join(&b.path), b.min_part_km2))
            .transpose()
    }

    /// Union of the tile footprints.
    pub fn tiles_bbox(&self, origin: &Origin) -> Option<Bbox> {
        self.tiles.iter().map(|t| t.bbox(origin)).reduce(|a, b| a.union(&b))
    }

    /// Union of the tile footprints plus the input read buffer; what `fetch` must cover.
    pub fn read_bbox(&self, origin: &Origin) -> Option<Bbox> {
        self.tiles_bbox(origin).map(|b| b.buffered(TILE_BUFFER))
    }
}
