use serde::Deserialize;
use crate::classify::categories::{Filter, MinzoomRule};
use crate::engine::extract::Producer;

#[derive(Debug, Deserialize)]
pub struct TopicSpec {
    pub table: String,
    /// Ordered transform pipeline. Each entry is either a bare string naming a no-arg
    /// tag transform (e.g. "lifecycle") or a parameterized object such as
    /// `{ "transform": "split_sides", "sides": [{ "highway": ..., "prefix": ... }] }`.
    #[serde(default)]
    pub transforms: Vec<Transform>,
    pub osm_fields: Vec<OsmFieldSpec>,
    pub sanitized_fields: Vec<SanitizedField>,
    /// Optional Filter condition evaluated against raw way tags before categorization.
    /// If the condition matches, the way is skipped entirely for this topic.
    /// Uses the same Filter JSON syntax as category conditions.
    #[serde(default)]
    pub exclude_condition: Option<Filter>,
    /// Topic-level default minzoom rule, used for any category without its own `minzoom`.
    #[serde(default)]
    pub minzoom: Option<MinzoomRule>,
}

/// One entry in a topic's `transforms` list.
/// A JSON string is a no-arg tag transform; a JSON object is a parameterized transform.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum Transform {
    /// e.g. "lifecycle", "cycleway_opposite", "construction_prefix", "cycleway_both".
    Named(String),
    /// One center-line split: unnest tags with `prefix` onto a side object whose effective
    /// highway becomes `highway`. List the entry once per projection, e.g.
    /// `{ "transform": "split_sides", "highway": "cycleway", "prefix": "cycleway" }`.
    SplitSides { transform: String, highway: String, prefix: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TagSource {
    Obj,
    Parent,
    ObjThenParent,
}

#[derive(Debug, Deserialize)]
pub struct OsmFieldSpec {
    pub output: String,
    /// Resolved from either `"key": "..."` or `"keys": [...]` in JSON.
    #[serde(flatten)]
    pub keys: OsmKeys,
    pub source: TagSource,
}

/// Allows `"key": "foo"` or `"keys": ["foo", "bar"]` in JSON.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum OsmKeys {
    Single { key: String },
    Multi  { keys: Vec<String> },
}

impl OsmKeys {
    pub fn as_slice(&self) -> &[String] {
        match self {
            OsmKeys::Single { key } => std::slice::from_ref(key),
            OsmKeys::Multi  { keys } => keys.as_slice(),
        }
    }
}

/// One produced field written to the merged `derived` column. Either a single-output field
/// backed by a `Producer` (extract [+ sanitize] / fallback / single-value derive), or the
/// two-output `traffic_mode` deriver.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum SanitizedField {
    /// `{ "output_left": ..., "output_right": ..., "derive": "traffic_mode" }`
    TrafficMode { output_left: String, output_right: String, derive: String },
    /// `{ "output": ..., "source": <Producer> }`
    Produce { output: String, source: Producer },
}
