use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    #[serde(rename = "self")]
    Self_,
    Left,
    Right,
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
