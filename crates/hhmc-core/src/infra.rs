//! Infrastructure: config/infra.toml and the prepared per-tile structure lists
//! (`data/prepared/infra/<rx>_<rz>.json`), written by prep and read by the infra stage
//! and QA.

use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::{Paths, TileKey};

/// A polygon: exterior ring followed by holes, EPSG:25832.
pub type Rings = Vec<Vec<(f64, f64)>>;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Deck {
    /// ALKIS `gml:id`.
    pub id: String,
    /// Index into `InfraConfig::deck`.
    pub kind: usize,
    pub polygons: Vec<Rings>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Pier {
    /// ALKIS `gml:id`, or `support:<deck id>:<n>` for generated supports.
    pub id: String,
    pub generated: bool,
    pub polygons: Vec<Rings>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Track {
    /// ALKIS `gml:id` of the AX_Gleis (a track split by tunnels keeps its id).
    pub id: String,
    /// Points at most 0.5 m apart.
    pub line: Vec<(f64, f64)>,
    /// Whether the track enters a tunnel at its first / last point (an ALKIS tunnel
    /// within a few meters): the track runs down a ramp to the portal there.
    #[serde(default)]
    pub portal_start: bool,
    #[serde(default)]
    pub portal_end: bool,
}

/// Structures touching a tile's read area.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct TileInfra {
    pub decks: Vec<Deck>,
    pub piers: Vec<Pier>,
    pub tracks: Vec<Track>,
    /// Footprints of ALKIS buildings above ground that overlap decks: bDOM shows their
    /// roofs there, not the deck.
    #[serde(default)]
    pub occluders: Vec<Rings>,
}

impl TileInfra {
    pub fn path(paths: &Paths, t: TileKey) -> PathBuf {
        paths.prepared().join("infra").join(format!("{}_{}.json", t.rx, t.rz))
    }

    pub fn load(path: &Path) -> Result<Self> {
        Ok(serde_json::from_str(&std::fs::read_to_string(path)?)?)
    }
}

/// config/infra.toml.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InfraConfig {
    pub heights: Heights,
    pub deck: Vec<DeckKind>,
    pub piers: Piers,
    pub surfaces: Surfaces,
    pub rails: Rails,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Surfaces {
    pub classes: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Heights {
    pub median_radius_m: f64,
    pub max_radius_m: f64,
    pub min_samples: usize,
    pub quantile: f64,
    pub min_rise_m: f64,
    pub smooth_radius_m: f64,
    pub anchor_tolerance_m: f64,
    pub anchor_blend_m: f64,
    pub anchor_step_m: f64,
    pub bank_gap_m: f64,
    pub abutment_step_m: f64,
    pub abutment_max_m: f64,
    pub abutment_min_rise_m: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeckKind {
    pub kind: String,
    pub bauwerksfunktion: Vec<i64>,
    #[serde(default)]
    pub surface_from_landuse: bool,
    pub top: String,
    pub structure: String,
    pub thickness: i32,
    pub railing: String,
    pub railing_min_drop: i32,
    pub min_clearance: i32,
    pub support_spacing_m: f64,
    #[serde(default)]
    pub pass_through_clearance: i32,
    #[serde(default)]
    pub floating: Option<Floating>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Floating {
    pub freeboard: i32,
    pub draft: i32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Piers {
    pub bauwerksfunktion: Vec<i64>,
    pub block: String,
    pub support_block: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rails {
    pub block: String,
    pub ballast: String,
    pub tunnel_bauwerksfunktion: Vec<i64>,
    pub max_gap_m: f64,
    pub viaduct_fill_m: f64,
    pub viaduct_classes: Vec<String>,
    /// Deck kind (`kind` of a `[[deck]]`) for track gaps between decks.
    pub gap_deck: String,
    pub bed_window: usize,
    pub corridor_m: f64,
}

impl InfraConfig {
    pub fn path(paths: &Paths) -> PathBuf {
        paths.config().join("infra.toml")
    }

    pub fn load(paths: &Paths) -> Result<Self> {
        let c: Self = crate::config::load_toml(&Self::path(paths))?;
        anyhow::ensure!(
            c.deck.iter().any(|d| d.kind == c.rails.gap_deck),
            "infra.toml: rails.gap_deck {:?} is not a deck kind",
            c.rails.gap_deck
        );
        Ok(c)
    }

    /// Deck kind index of an ALKIS `bauwerksfunktion`.
    pub fn deck_kind(&self, bwf: i64) -> Option<usize> {
        self.deck.iter().position(|d| d.bauwerksfunktion.contains(&bwf))
    }
}

/// World block columns within `radius` (m) of a track: the vegetation keeps its clearance
/// there.
pub fn track_corridor(
    tracks: &[Track],
    origin: &crate::config::Origin,
    radius: f64,
) -> std::collections::HashSet<(i32, i32)> {
    let mut out = std::collections::HashSet::new();
    let k = radius.ceil() as i32;
    for t in tracks {
        for &(e, n) in &t.line {
            let (x0, z0) = origin.block_at(e, n);
            for dz in -k..=k {
                for dx in -k..=k {
                    let (ce, cn) = origin.block_center(x0 + dx, z0 + dz);
                    if (ce - e).hypot(cn - n) <= radius {
                        out.insert((x0 + dx, z0 + dz));
                    }
                }
            }
        }
    }
    out
}

/// Even-odd point-in-polygon over all rings.
pub fn contains(rings: &Rings, e: f64, n: f64) -> bool {
    let mut inside = false;
    for r in rings {
        for i in 0..r.len().saturating_sub(1) {
            let ((x1, y1), (x2, y2)) = (r[i], r[i + 1]);
            if (y1 > n) != (y2 > n) && e < x1 + (n - y1) * (x2 - x1) / (y2 - y1) {
                inside = !inside;
            }
        }
    }
    inside
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_parses() {
        let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config/infra.toml");
        let c: InfraConfig = crate::config::load_toml(&p).unwrap();
        assert_eq!(c.deck_kind(1830).map(|k| c.deck[k].kind.as_str()), Some("elevated"));
        assert!(c.deck[c.deck_kind(1810).unwrap()].floating.is_some());
    }

    #[test]
    fn point_in_polygon_with_hole() {
        let outer = vec![(0.0, 0.0), (10.0, 0.0), (10.0, 10.0), (0.0, 10.0), (0.0, 0.0)];
        let hole = vec![(4.0, 4.0), (6.0, 4.0), (6.0, 6.0), (4.0, 6.0), (4.0, 4.0)];
        let r = vec![outer, hole];
        assert!(contains(&r, 1.0, 1.0));
        assert!(!contains(&r, 5.0, 5.0));
        assert!(!contains(&r, 11.0, 5.0));
    }
}
