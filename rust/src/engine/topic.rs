use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct TopicSpec {
    pub table: String,
    pub transformations: Vec<TransformSpec>,
    pub osm_fields: Vec<OsmFieldSpec>,
    pub sanitized_fields: Vec<SanitizerSpec>,
    /// Named Rust exclusion functions applied before categorization.
    /// Supported: "by_access", "by_service"
    #[serde(default)]
    pub exclude_fns: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct TransformSpec {
    pub highway: String,
    pub prefix: String,
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

/// Each variant corresponds to a named sanitizer function.
/// Serde tag = "fn" field in JSON.
#[derive(Debug, Deserialize)]
#[serde(tag = "fn", rename_all = "snake_case")]
pub enum SanitizerSpec {
    TrafficSign          { output: String },
    Separation           { output: String, side: String },
    Marking              { output: String, side: String },
    Buffer               { output: String, side: String },
    SurfaceColor         { output: String },
    YesFlag              { output: String, key: String, source: TagSource },
    ParseLength          { output: String, key: String },
    Lifecycle            { output: String },
    SurfaceWithFallback  { output: String },
    SmoothnessWithFallback { output: String },
    DeriveOneway         { output: String },
    DeriveTrafficMode    { output_left: String, output_right: String },
}
