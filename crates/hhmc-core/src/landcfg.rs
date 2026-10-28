//! `config/blocks.toml` (surface classes and ALKIS rules) and `config/water.toml`.

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use crate::config::{Paths, load_toml};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BlocksConfig {
    pub subsoil_depth: i32,
    pub deep: String,
    pub floor: String,
    pub fallback: String,
    #[serde(rename = "class")]
    pub classes: Vec<SurfaceClass>,
    #[serde(rename = "rule")]
    pub rules: Vec<LandUseRule>,
    pub buildings: BuildingStyle,
}

/// `[buildings]` in config/blocks.toml.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BuildingStyle {
    pub wall: String,
    pub floor: String,
    pub flat_roof: String,
    pub flat_roof_slab: String,
    pub roof: String,
    pub roof_slab: String,
    pub roof_stairs: String,
    pub flat_max_deg: f64,
    pub stairs_min_deg: f64,
    pub stairs_max_deg: f64,
    pub story_blocks: i32,
    pub foundation_max_blocks: i32,
    pub canopy_functions: Vec<String>,
    pub canopy_post_spacing: i32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SurfaceClass {
    pub id: String,
    pub top: String,
    pub subsoil: String,
    pub biome: String,
    #[serde(default)]
    pub water: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LandUseRule {
    pub nutzart: String,
    #[serde(default)]
    pub bez: Option<String>,
    pub class: String,
}

impl BlocksConfig {
    pub fn load(paths: &Paths) -> Result<Self> {
        let c: Self = load_toml(&paths.config().join("blocks.toml"))?;
        c.validate()?;
        Ok(c)
    }

    fn validate(&self) -> Result<()> {
        if self.classes.is_empty() || self.classes.len() > 255 {
            bail!("blocks.toml: need 1..=255 classes");
        }
        self.class_index(&self.fallback)?;
        for r in &self.rules {
            self.class_index(&r.class)?;
        }
        Ok(())
    }

    pub fn class_index(&self, id: &str) -> Result<u8> {
        match self.classes.iter().position(|c| c.id == id) {
            Some(i) => Ok(i as u8),
            None => bail!("blocks.toml: unknown class `{id}`"),
        }
    }

    /// Class for an ALKIS land-use polygon; first matching rule wins.
    pub fn classify(&self, nutzart: &str, bez: Option<&str>) -> u8 {
        let id = self
            .rules
            .iter()
            .find(|r| r.nutzart == nutzart && r.bez.as_deref().is_none_or(|b| Some(b) == bez))
            .map_or(self.fallback.as_str(), |r| r.class.as_str());
        self.class_index(id).expect("validated")
    }

    pub fn is_water(&self, class: u8) -> bool {
        self.classes.get(class as usize).is_some_and(|c| c.water)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WaterConfig {
    pub default_depth_m: f64,
    pub blend_m: f64,
    pub gap_bed_max_nhn: f64,
    pub gap_flat_tol_m: f64,
    pub gap_max_m: f64,
    pub flood_max_m: f64,
    /// Unseeded water whose shoreline DGM1 spans more than this (p10–p90, m) is split
    /// into level bands of `stream_band_m`; bands smaller than `stream_min_piece_cells`
    /// join their neighbors.
    pub stream_split_spread_m: f64,
    /// Quantile of an unseeded component's shoreline DGM1 used as its level.
    pub unseeded_level_quantile: f64,
    pub stream_band_m: f64,
    pub stream_min_piece_cells: usize,
    #[serde(rename = "body")]
    pub bodies: Vec<WaterBody>,
    #[serde(rename = "lock", default)]
    pub locks: Vec<Lock>,
    /// Barrier disk radius (m) per OSM water structure kind (`lock_gate`, `lock`, `weir`,
    /// `sluice_gate`, `dam`); kinds not listed are not barriers.
    #[serde(default)]
    pub osm_barriers: std::collections::BTreeMap<String, f64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WaterBody {
    pub id: String,
    pub description: String,
    pub level_nhn: f64,
    pub level_kind: String,
    pub source: String,
    pub confidence: String,
    #[serde(default)]
    pub seeds: Vec<[f64; 2]>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Lock {
    pub name: String,
    pub center: [f64; 2],
    pub radius_m: f64,
}

impl WaterConfig {
    pub fn load(paths: &Paths) -> Result<Self> {
        load_toml(&paths.config().join("water.toml"))
    }
}

/// `data/prepared/water/bodies.json`: what each value of the water rasters means.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BodiesFile {
    pub bodies: Vec<BodyInfo>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BodyInfo {
    /// Value in the water rasters (1-based; 0 = dry).
    pub index: u16,
    /// Body id from config/water.toml, or `auto-<n>` for unseeded components.
    pub id: String,
    pub level_nhn: f64,
    /// "config" or "median DGM1 along shoreline".
    pub level_source: String,
    pub cells: usize,
    pub centroid: [f64; 2],
}

impl BodiesFile {
    pub fn load(paths: &Paths) -> Result<Self> {
        let p = paths.prepared().join("water/bodies.json");
        let text = std::fs::read_to_string(&p)
            .map_err(|e| anyhow::anyhow!("reading {}: {e}; run `hhmc prep`", p.display()))?;
        Ok(serde_json::from_str(&text)?)
    }

    /// Level per raster value (index 0 = dry = `None`).
    pub fn levels(&self) -> Vec<Option<f64>> {
        let n = self.bodies.iter().map(|b| b.index as usize).max().unwrap_or(0);
        let mut v = vec![None; n + 1];
        for b in &self.bodies {
            v[b.index as usize] = Some(b.level_nhn);
        }
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_configs_parse_and_classify() {
        let paths =
            Paths { root: std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..") };
        let b = BlocksConfig::load(&paths).unwrap();
        let water = b.class_index("water").unwrap();
        assert_eq!(b.classify("Fliessgewaesser", Some("Kanal")), water);
        assert!(b.is_water(water));
        assert_eq!(b.classify("Strassenverkehr", Some("Fahrbahn")), b.class_index("road").unwrap());
        assert_eq!(
            b.classify("Strassenverkehr", Some("Begleitfläche Straßenverkehr")),
            b.class_index("sidewalk").unwrap()
        );
        assert_eq!(b.classify("Platz", None), b.class_index("plaza").unwrap());
        assert_eq!(b.classify("Something New", None), b.class_index("built").unwrap());
        let w = WaterConfig::load(&paths).unwrap();
        assert!(w.bodies.iter().any(|b| b.id == "elbe_tidal"));
        // Every block named in blocks.toml exists in the pinned version (block_colors.toml is
        // derived from its client jar).
        let colors = std::fs::read_to_string(paths.config().join("block_colors.toml")).unwrap();
        let s = &b.buildings;
        let mut names: Vec<&str> = vec![
            &b.deep,
            &b.floor,
            &s.wall,
            &s.floor,
            &s.flat_roof,
            &s.flat_roof_slab,
            &s.roof,
            &s.roof_slab,
            &s.roof_stairs,
        ];
        for c in &b.classes {
            names.push(&c.top);
            names.push(&c.subsoil);
        }
        for n in names {
            assert!(colors.contains(&format!("\"{n}\"")), "unknown block {n}");
        }
    }
}
