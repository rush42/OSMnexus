use serde::Deserialize;
use crate::classify::categories::{Filter, MinzoomRule};
use crate::engine::extract::{Producer, TagSet};

#[derive(Debug, Deserialize)]
pub struct TopicSpec {
    pub table: String,
    /// Ordered transform pipeline. Each entry is either a bare string naming a no-arg
    /// tag transform (e.g. "lifecycle") or a parameterized object such as
    /// `{ "transform": "split_sides", "sides": [{ "highway": ..., "prefix": ... }] }`.
    #[serde(default)]
    pub transforms: Vec<Transform>,
    pub osm_fields: Vec<OsmFieldSpec>,
    /// Simple field sanitizers: read one tag (or first present of several), clean it with a
    /// named `&str -> atomic` sanitizer, write to `tag`.
    #[serde(default)]
    pub sanitizers: Vec<Sanitizer>,
    /// Computed fields needing more than a single read + sanitize: multi-source fallbacks
    /// (lifecycle/surface/smoothness) and Rust derivers (oneway/traffic_mode).
    #[serde(default)]
    pub derivers: Vec<SanitizedField>,
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

/// A raw OSM tag copied into the `osm` column, produced by the same extraction layer as
/// sanitized fields (`{ "output": ..., "source": <Producer> }`). obj-then-parent is just a
/// `fallback` of two extracts.
#[derive(Debug, Deserialize)]
pub struct OsmFieldSpec {
    pub output: String,
    pub source: Producer,
}

/// A simple sanitizer: `{ tag, name, in?, from? }`. Reads the first present of `in` (default
/// `[tag]`) from `from` (default obj), applies the `name` sanitizer, writes to `tag`.
#[derive(Debug, Deserialize)]
pub struct Sanitizer {
    pub tag: String,
    pub name: String,
    #[serde(default, rename = "in")]
    pub in_keys: Option<StringOrVec>,
    #[serde(default)]
    pub from: TagSet,
}

/// Accepts either `"foo"` or `["foo", "bar"]` in JSON.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum StringOrVec {
    One(String),
    Many(Vec<String>),
}

impl Sanitizer {
    fn input_keys(&self) -> Vec<String> {
        match &self.in_keys {
            Some(StringOrVec::One(s)) => vec![s.clone()],
            Some(StringOrVec::Many(v)) => v.clone(),
            None => vec![self.tag.clone()],
        }
    }

    /// Desugar to the equivalent `Produce` field so sanitizers and derivers share one eval path.
    pub fn to_field(&self) -> SanitizedField {
        SanitizedField::Produce {
            output: self.tag.clone(),
            source: Producer::Extract {
                key: None,
                keys: Some(self.input_keys()),
                from: self.from,
                side: None,
                sanitize: Some(self.name.clone()),
                if_copy_surface: false,
            },
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
