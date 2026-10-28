//! Trees: the prepared per-tile tree lists (written by prep, read by the vegetation stage
//! and QA) and config/vegetation.toml.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::{Paths, TILE_SIZE, TileKey};

/// Where a tree's height comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HeightSource {
    /// `Hoehe_aus_ALS` of the 3D street tree cadaster.
    Als,
    /// Median height of 3D-cadaster trees of the genus with a similar crown diameter.
    Crown,
    /// As `Crown`, from the trunk girth.
    Girth,
    /// Median height of the genus (or all trees).
    Genus,
    /// Canopy height model maximum, calibrated against the ALS heights.
    Canopy,
}

/// Where a tree comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TreeSource {
    /// BUKEA street tree cadaster (`trees`).
    Street,
    /// HPA port street tree cadaster (`trees_hpa`).
    Port,
    /// Canopy maximum in a park or wood.
    Canopy,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Tree {
    /// Unique and stable: `street:<baumid>`, `port:<baumnummer>`, `canopy:<e>_<n>`.
    pub id: String,
    pub source: TreeSource,
    /// Trunk position, EPSG:25832 (after any conflict snap).
    pub e: f64,
    pub n: f64,
    /// Latin genus (first word of the species), empty if unknown.
    pub genus: String,
    pub height_m: f32,
    pub height_source: HeightSource,
    pub crown_m: f32,
    pub girth_cm: Option<f32>,
    /// Meters the trunk was moved off a blocked cell, 0 if not.
    pub snapped_m: f32,
}

/// A cadaster tree that is not placed, and why.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Conflict {
    pub id: String,
    pub e: f64,
    pub n: f64,
    /// `building`, `water` or a blocked surface class id.
    pub reason: String,
}

/// `data/prepared/trees/<rx>_<rz>.json`: trees whose trunk lies in the tile (`own`), the
/// trees of neighboring tiles whose crown may reach into it (`near`), and the cadaster
/// trees of the tile that were not placed.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct TileTrees {
    pub own: Vec<Tree>,
    pub near: Vec<Tree>,
    pub conflicts: Vec<Conflict>,
}

impl TileTrees {
    pub fn path(paths: &Paths, t: TileKey) -> PathBuf {
        paths.prepared().join("trees").join(format!("{}_{}.json", t.rx, t.rz))
    }

    pub fn load(path: &Path) -> Result<Self> {
        Ok(serde_json::from_str(&std::fs::read_to_string(path)?)?)
    }
}

/// config/vegetation.toml.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VegetationConfig {
    pub default_family: String,
    /// Latin genus → family id.
    pub genus: BTreeMap<String, String>,
    pub family: BTreeMap<String, Family>,
    pub tree: TreeShape,
    pub conflicts: Conflicts,
    pub pit: Pit,
    pub parks: Parks,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Family {
    pub log: String,
    pub leaves: String,
    pub shape: CrownShape,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CrownShape {
    Ellipsoid,
    Cone,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TreeShape {
    pub crown_share: f32,
    pub min_crown_base_blocks: i32,
    pub big_trunk_girth_cm: f32,
    pub edge_density: f32,
    pub default_crown_m: f32,
    pub min_crown_m: f32,
    pub min_height_m: f32,
    pub max_height_m: f32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Conflicts {
    pub blocked_classes: Vec<String>,
    pub snap_m: f64,
    pub clearance_classes: Vec<String>,
    pub clearance_blocks: i32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Pit {
    pub block: String,
    pub radius_blocks: i32,
    pub classes: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Parks {
    pub enabled: bool,
    pub classes: Vec<String>,
    pub min_height_m: f32,
    pub suppress_factor: f32,
    pub building_clearance_m: f64,
    pub min_crown_m: f32,
    pub max_crown_m: f32,
}

impl VegetationConfig {
    pub fn path(paths: &Paths) -> PathBuf {
        paths.config().join("vegetation.toml")
    }

    pub fn load(paths: &Paths) -> Result<Self> {
        let c: Self = crate::config::load_toml(&Self::path(paths))?;
        anyhow::ensure!(
            c.family.contains_key(&c.default_family),
            "vegetation.toml: default_family {:?} is not a family",
            c.default_family
        );
        for (g, f) in &c.genus {
            anyhow::ensure!(
                c.family.contains_key(f),
                "vegetation.toml: genus {g}: no family {f:?}"
            );
        }
        Ok(c)
    }

    /// Family of a Latin genus.
    pub fn family_of(&self, genus: &str) -> &str {
        self.genus.get(genus).unwrap_or(&self.default_family)
    }
}

/// Latin genus from a cadaster species text such as `"Platanus acerifolia / Ahornblättrige
/// Platane"` or `"Acer platanoides"`: the first word, if it starts with a capital letter.
pub fn genus_of(species: &str) -> String {
    let w = species.split_whitespace().next().unwrap_or("");
    if w.chars().next().is_some_and(char::is_uppercase) && w.chars().all(char::is_alphabetic) {
        w.to_string()
    } else {
        String::new()
    }
}

/// A tree of the street tree cadasters, as published.
#[derive(Clone, Debug, PartialEq)]
pub struct CadasterTree {
    /// `street:<baumid>` or `port:<baumnummer>`.
    pub id: String,
    pub source: TreeSource,
    /// Tree number (BUKEA), the key of the 3D cadaster.
    pub number: Option<String>,
    pub e: f64,
    pub n: f64,
    pub genus: String,
    pub crown: Option<f32>,
    /// Trunk circumference in cm (from the diameter for the port cadaster).
    pub girth: Option<f32>,
}

/// Reads the cadasters' WFS GeoJSON files (BUKEA `strassenbaumkataster` for
/// [`TreeSource::Street`], HPA `strbaeume_hpa` for [`TreeSource::Port`]), sorted by id
/// with repeated features dropped.
pub fn load_cadaster(files: &[(TreeSource, PathBuf)]) -> Result<Vec<CadasterTree>> {
    use anyhow::Context;
    use serde_json::{Map, Value};
    #[derive(Deserialize)]
    struct Fc {
        // The WFS leaves the member out when nothing matches.
        #[serde(default)]
        features: Vec<Feature>,
    }
    #[derive(Deserialize)]
    struct Feature {
        geometry: Option<Geometry>,
        properties: Map<String, Value>,
    }
    #[derive(Deserialize)]
    struct Geometry {
        coordinates: Value,
    }
    fn point(g: &Option<Geometry>) -> Option<(f64, f64)> {
        let mut v = &g.as_ref()?.coordinates;
        // Point or MultiPoint: descend to the first coordinate pair.
        while let Some(first) = v.as_array()?.first().filter(|f| f.is_array()) {
            v = first;
        }
        let a = v.as_array()?;
        Some((a.first()?.as_f64()?, a.get(1)?.as_f64()?))
    }
    fn num(p: &Map<String, Value>, k: &str) -> Option<f32> {
        match p.get(k)? {
            Value::Number(n) => n.as_f64().map(|x| x as f32),
            Value::String(s) => s.trim().parse().ok(),
            _ => None,
        }
    }
    fn text(p: &Map<String, Value>, k: &str) -> Option<String> {
        match p.get(k)? {
            Value::String(s) if !s.trim().is_empty() => Some(s.trim().to_string()),
            Value::Number(n) => Some(n.to_string()),
            _ => None,
        }
    }
    let mut out: Vec<CadasterTree> = Vec::new();
    let mut ids = std::collections::HashSet::new();
    for (source, path) in files {
        let fc: Fc = serde_json::from_reader(std::io::BufReader::new(
            std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?,
        ))
        .with_context(|| format!("parsing {}", path.display()))?;
        for ft in fc.features {
            let Some((e, n)) = point(&ft.geometry) else { continue };
            let at = || format!("@{e:.2},{n:.2}");
            let p = &ft.properties;
            // A tree without its number (a few port trees) is identified by its position
            // (`at`).
            let t = match source {
                TreeSource::Port => CadasterTree {
                    id: format!("port:{}", text(p, "baumnummer").unwrap_or_else(at)),
                    source: *source,
                    number: None,
                    e,
                    n,
                    genus: genus_of(&text(p, "baumart").unwrap_or_default()),
                    crown: num(p, "kronendurchmesser"),
                    // Trunk diameter (cm) → circumference.
                    girth: num(p, "stammdurchmesser").map(|d| d * std::f32::consts::PI),
                },
                _ => CadasterTree {
                    id: format!("street:{}", text(p, "baumid").unwrap_or_else(at)),
                    source: *source,
                    number: text(p, "baumnummer"),
                    e,
                    n,
                    genus: genus_of(&text(p, "gattung_latein").unwrap_or_default()),
                    crown: num(p, "kronendurchmesser"),
                    girth: num(p, "stammumfang"),
                },
            };
            // Paged or overlapping requests may repeat a tree.
            if ids.insert(t.id.clone()) {
                out.push(t);
            }
        }
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(out)
}

/// Highest building block per tile column from a building layer's clears and blocks.
pub fn building_tops(clears: &[(u16, u16, i16, i16)], blocks: &[(u16, i16, u16, u16)]) -> Vec<i16> {
    let size = TILE_SIZE as usize;
    let mut top = vec![i16::MIN; size * size];
    for &(x, z, _, y1) in clears {
        let k = z as usize * size + x as usize;
        top[k] = top[k].max(y1);
    }
    for &(x, y, z, _) in blocks {
        let k = z as usize * size + x as usize;
        top[k] = top[k].max(y);
    }
    top
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn genus_parsing() {
        assert_eq!(genus_of("Platanus acerifolia / Ahornblättrige Platane"), "Platanus");
        assert_eq!(genus_of("Acer platanoides"), "Acer");
        assert_eq!(genus_of("Populus / Pappel"), "Populus");
        assert_eq!(genus_of(""), "");
        assert_eq!(genus_of("unbekannt"), "");
    }
}
