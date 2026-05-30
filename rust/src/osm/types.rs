use std::collections::HashMap;

pub type RawTags = HashMap<String, String>;

pub struct OsmWay {
    pub id: i64,
    /// WGS84 coordinates in (lon, lat) order.
    pub coords: Vec<(f64, f64)>,
    pub tags: RawTags,
    pub meta: WayMeta,
}

pub struct WayMeta {
    /// Unix timestamp (seconds since epoch), if available.
    pub timestamp: Option<i64>,
    pub user: Option<String>,
    pub changeset: Option<i64>,
}
