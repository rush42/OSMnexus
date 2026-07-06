use serde::Deserialize;
use crate::classify::categories::{Filter, MinzoomRule};
use crate::classify::sanitize::StrOrVec;
use crate::engine::extract::{Producer, TagSet};

#[derive(Debug, Deserialize)]
pub struct TopicSpec {
    pub table: String,
    /// Ordered transform pipeline. Each entry is either a bare string naming a no-arg
    /// tag transform (e.g. "lifecycle") or a parameterized object such as
    /// `{ "transform": "split_sides", "sides": [{ "highway": ..., "prefix": ... }] }`.
    #[serde(default)]
    pub transforms: Vec<Transform>,
    pub osm_fields: Vec<Field>,
    /// Simple field sanitizers: read one tag (or first present of several), clean it with a
    /// named `&str -> atomic` sanitizer, write to `tag`.
    #[serde(default)]
    pub sanitizers: Vec<Sanitizer>,
    /// References (by name) into the topic's `derivers.json` library. Each binding names a
    /// single-output deriver and the output field it writes. Categories may override these
    /// by re-binding a different deriver to the same output.
    #[serde(default)]
    pub derivers: Vec<DeriverBinding>,
    /// Optional Filter condition evaluated against raw way tags before categorization.
    /// If the condition matches, the way is skipped entirely for this topic.
    /// Uses the same Filter JSON syntax as category conditions.
    #[serde(default)]
    pub exclude_condition: Option<Filter>,
    /// Topic-level default minzoom rule, used for any category without its own `minzoom`.
    #[serde(default)]
    pub minzoom: Option<MinzoomRule>,
    /// Topic-level default constants seeded into `derived` (lowest priority — any sanitizer/
    /// deriver producing the same key overrides them). Categories override per-key via their own
    /// `consts`.
    #[serde(default)]
    pub consts: serde_json::Map<String, serde_json::Value>,
    /// Topic-level default private metadata, emitted into the `private` column. Categories override
    /// per-key via their own `private`. The private counterpart to `consts`.
    #[serde(default)]
    pub private: serde_json::Map<String, serde_json::Value>,
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

/// One produced field: `{ "output": ..., "source": <Producer> }`. Used for `osm_fields`,
/// desugared sanitizers, and resolved derivers alike — they all share one eval path.
#[derive(Debug, Clone, Deserialize)]
pub struct Field {
    pub output: String,
    pub source: Producer,
}

/// A reference from `topic.json` (or a category) into the `derivers.json` library.
/// A bare string names a deriver whose output equals its name; the object form binds a
/// deriver to a different output (e.g. `surface_from_parent` → `surface`).
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum DeriverBinding {
    Named(String),
    Bound { deriver: String, output: String },
}

impl DeriverBinding {
    pub fn deriver(&self) -> &str {
        match self { DeriverBinding::Named(s) => s, DeriverBinding::Bound { deriver, .. } => deriver }
    }
    pub fn output(&self) -> &str {
        match self { DeriverBinding::Named(s) => s, DeriverBinding::Bound { output, .. } => output }
    }
}

/// A simple sanitizer: `{ tag, name, in?, from? }`. Reads the first present of `in` (default
/// `[tag]`) from `from` (default obj), applies the `name` sanitizer, writes to `tag`.
#[derive(Debug, Deserialize)]
pub struct Sanitizer {
    pub tag: String,
    pub name: String,
    #[serde(default, rename = "in")]
    pub in_keys: Option<StrOrVec>,
    #[serde(default)]
    pub from: TagSet,
}

impl Sanitizer {
    fn input_keys(&self) -> Vec<String> {
        match self.in_keys.clone() {
            Some(sv) => sv.into_vec(),
            None => vec![self.tag.clone()],
        }
    }

    /// Desugar to the equivalent `Field` so sanitizers and derivers share one eval path.
    pub fn to_field(&self) -> Field {
        Field {
            output: self.tag.clone(),
            source: Producer::Extract {
                key: None,
                keys: Some(self.input_keys()),
                from: self.from,
                side: None,
                sanitize: Some(self.name.clone()),
                consts: serde_json::Map::new(),
            },
        }
    }
}
