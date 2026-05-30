use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    #[serde(rename = "self")]
    Self_,
    Left,
    Right,
}

/// Raw OSM tags for a bikelane object — only values that came directly from the PBF.
#[derive(Debug, Serialize)]
pub struct BikelaneOsmTags {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub surface: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub smoothness: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub width: Option<f32>,
    #[serde(rename = "source:width", skip_serializing_if = "Option::is_none")]
    pub source_width: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bridge: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tunnel: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub oneway: Option<String>,
    #[serde(rename = "oneway:bicycle", skip_serializing_if = "Option::is_none")]
    pub oneway_bicycle: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub traffic_sign: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub informal: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub covered: Option<bool>,
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
}

/// Values computed by the pipeline — nothing that came verbatim from OSM.
#[derive(Debug, Serialize)]
pub struct BikelaneDerived {
    pub id: String,
    pub category: &'static str,
    pub side: Side,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prefix: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_highway: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub road: Option<String>,
    pub length_m: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lifecycle: Option<String>,
}

/// Raw OSM tags for a road object.
#[derive(Debug, Serialize)]
pub struct RoadOsmTags {
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
    pub bridge: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tunnel: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub operator_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub informal: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub covered: Option<bool>,
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
#[derive(Debug, Serialize)]
pub struct OsmMeta {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_by: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub changeset_id: Option<i64>,
}
