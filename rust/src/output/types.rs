use std::collections::HashMap;
use serde::Serialize;

/// Type alias used throughout — same as RawTags but avoids circular imports.
pub type RawTagsRef = HashMap<String, String>;

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    #[serde(rename = "self")]
    Self_,
    Left,
    Right,
}

// ── Road structs (stay typed — roads don't go through the topic engine) ───────

#[derive(Debug, Serialize)]
pub struct RoadOsm {
    pub highway: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(rename = "ref", skip_serializing_if = "Option::is_none")]
    pub name_ref: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub surface: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub smoothness: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub maxspeed: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub oneway: Option<String>,
    #[serde(rename = "oneway:bicycle", skip_serializing_if = "Option::is_none")]
    pub oneway_bicycle: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lit: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bridge: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tunnel: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub operator_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub informal: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub covered: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub traffic_sign: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct RoadSanitized {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bridge: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tunnel: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub traffic_sign: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct RoadDerived {
    pub id: String,
    pub road: String,
    pub length_m: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lifecycle: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bikelane_left: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bikelane_right: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bikelane_self: Option<String>,
}

/// OSM metadata extracted from the element.
#[derive(Debug, Clone, Serialize)]
pub struct OsmMeta {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_by: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub changeset_id: Option<i64>,
}
