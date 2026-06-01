use super::types::{BikelaneDerived, BikelaneOsm, BikelanePrivate, BikelaneSanitized, OsmMeta};
use geo::LineString;

pub struct BikelaneRow {
    pub osm_id: i64,
    pub osm_type: &'static str,
    pub id: String,
    pub osm: BikelaneOsm,
    pub sanitized: BikelaneSanitized,
    pub derived: BikelaneDerived,
    pub private: BikelanePrivate,
    pub meta: OsmMeta,
    pub geom: LineString<f64>,
    pub minzoom: i32,
}
