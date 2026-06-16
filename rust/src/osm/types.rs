use std::collections::HashMap;

use serde::Deserialize;

pub type RawTags = HashMap<String, String>;

pub struct OsmWay {
    pub id: i64,
    /// WGS84 coordinates in (lon, lat) order.
    pub coords: Vec<(f64, f64)>,
    /// Referenced node IDs, in order. Retained so a later graph step can split ways at
    /// shared (intersection) nodes without re-reading the PBF.
    pub node_ids: Vec<i64>,
    pub tags: RawTags,
    pub meta: WayMeta,
}

pub struct WayMeta {
    /// Unix timestamp (seconds since epoch), if available.
    pub timestamp: Option<i64>,
    pub user: Option<String>,
    pub changeset: Option<i64>,
}

/// A data-defined predicate over a way's tags, declared in each topic's `topic.json`
/// (`"element_filter": { "tag": "highway" }` or `{ "tag": "railway", "in": ["rail", ...] }`).
/// Presence-only when `any_of` is absent; otherwise the tag value must be in the list.
/// There is no hardcoded tag logic in Rust — the reader keeps any way matching *any*
/// topic's filter.
#[derive(Debug, Clone, Deserialize)]
pub struct ElementFilter {
    pub tag: String,
    #[serde(default, rename = "in")]
    pub any_of: Option<Vec<String>>,
}

impl ElementFilter {
    /// The default filter, used when a topic declares none: presence of `highway`.
    /// Keeps the existing three topics byte-identical without editing their JSON.
    pub fn highway() -> Self {
        ElementFilter { tag: "highway".to_owned(), any_of: None }
    }

    pub fn matches(&self, tags: &RawTags) -> bool {
        match (tags.get(&self.tag), &self.any_of) {
            (None, _) => false,
            (Some(_), None) => true,
            (Some(v), Some(allowed)) => allowed.iter().any(|a| a == v),
        }
    }
}

/// In-memory index of the nodes referenced by the kept ways. Foundation for a routable
/// graph: a node used by ≥2 kept ways is an intersection / crossing point.
pub struct NodeIndex {
    /// Coordinates (lon, lat as f32, ~1 m precision) for every referenced node.
    pub coords: HashMap<i64, (f32, f32)>,
    /// How many kept ways reference each node. `>= 2` ⇒ intersection / crossing.
    pub use_counts: HashMap<i64, u32>,
}
