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

// ── Bikelane structs ──────────────────────────────────────────────────────────

/// Raw OSM tag values exactly as they appear in the PBF — strings, colon-keyed.
/// Nothing is parsed, whitelisted, or transformed here.
#[derive(Debug, Serialize)]
pub struct BikelaneOsm {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub surface: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub smoothness: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub width: Option<String>,
    #[serde(rename = "source:width", skip_serializing_if = "Option::is_none")]
    pub source_width: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bridge: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tunnel: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub oneway: Option<String>,
    #[serde(rename = "oneway:bicycle", skip_serializing_if = "Option::is_none")]
    pub oneway_bicycle: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub traffic_sign: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub informal: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub covered: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub operator_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mapillary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub segregated: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bicycle: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub foot: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temporary: Option<String>,
    #[serde(rename = "separation:left", skip_serializing_if = "Option::is_none")]
    pub separation_left: Option<String>,
    #[serde(rename = "separation:right", skip_serializing_if = "Option::is_none")]
    pub separation_right: Option<String>,
    #[serde(rename = "separation:both", skip_serializing_if = "Option::is_none")]
    pub separation_both: Option<String>,
    #[serde(rename = "marking:left", skip_serializing_if = "Option::is_none")]
    pub marking_left: Option<String>,
    #[serde(rename = "marking:right", skip_serializing_if = "Option::is_none")]
    pub marking_right: Option<String>,
    #[serde(rename = "marking:both", skip_serializing_if = "Option::is_none")]
    pub marking_both: Option<String>,
    #[serde(rename = "traffic_mode:left", skip_serializing_if = "Option::is_none")]
    pub traffic_mode_left: Option<String>,
    #[serde(rename = "traffic_mode:right", skip_serializing_if = "Option::is_none")]
    pub traffic_mode_right: Option<String>,
    #[serde(rename = "traffic_mode:both", skip_serializing_if = "Option::is_none")]
    pub traffic_mode_both: Option<String>,
    #[serde(rename = "buffer:left", skip_serializing_if = "Option::is_none")]
    pub buffer_left: Option<String>,
    #[serde(rename = "buffer:right", skip_serializing_if = "Option::is_none")]
    pub buffer_right: Option<String>,
    #[serde(rename = "buffer:both", skip_serializing_if = "Option::is_none")]
    pub buffer_both: Option<String>,
    #[serde(rename = "surface:colour", skip_serializing_if = "Option::is_none")]
    pub surface_colour: Option<String>,
}

/// Tags after applying Lua-equivalent sanitization/whitelist functions — underscore-keyed.
#[derive(Debug, Serialize)]
pub struct BikelaneSanitized {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub traffic_sign: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub separation_left: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub separation_right: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub marking_left: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub marking_right: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub traffic_mode_left: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub traffic_mode_right: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub buffer_left: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub buffer_right: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub surface_color: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bridge: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tunnel: Option<bool>,
    /// DeriveOneway result: yes | no | car_not_bike | assumed_no | implicit_yes
    pub oneway: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub width: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub width_effective: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lifecycle: Option<String>,
    /// Surface after parent fallback (copy_surface_smoothness_from_parent logic).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub surface: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub smoothness: Option<String>,
}

/// Values computed purely by the pipeline — no raw OSM values here.
#[derive(Debug, Serialize)]
pub struct BikelaneDerived {
    pub id: String,
    pub category: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub road: Option<String>,
    pub length_m: f64,
}

/// Internal `_`-prefixed processing state (formerly filtered out in Lua before DB write).
#[derive(Debug, Serialize)]
pub struct BikelanePrivate {
    #[serde(rename = "_side")]
    pub side: Side,
    #[serde(rename = "_prefix", skip_serializing_if = "Option::is_none")]
    pub prefix: Option<&'static str>,
    #[serde(rename = "_infix", skip_serializing_if = "Option::is_none")]
    pub infix: Option<&'static str>,
    #[serde(rename = "_parent_highway", skip_serializing_if = "Option::is_none")]
    pub parent_highway: Option<String>,
    #[serde(rename = "_implicit_oneway_confidence")]
    pub implicit_oneway_confidence: &'static str,
}

// ── Road structs ──────────────────────────────────────────────────────────────

/// Raw OSM tags for a road object.
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

/// Sanitized road tags.
#[derive(Debug, Serialize)]
pub struct RoadSanitized {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bridge: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tunnel: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub traffic_sign: Option<String>,
}

/// Values computed by the pipeline for a road.
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

/// OSM metadata extracted from the element (version, timestamp, uid, user).
#[derive(Debug, Clone, Serialize)]
pub struct OsmMeta {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_by: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub changeset_id: Option<i64>,
}
