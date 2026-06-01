use super::types::{OsmMeta, RoadDerived, RoadOsm, RoadPrivate, RoadSanitized};
use geo::LineString;

pub struct RoadRow {
    pub osm_id: i64,
    pub osm_type: &'static str,
    pub id: String,
    pub osm: RoadOsm,
    pub sanitized: RoadSanitized,
    pub derived: RoadDerived,
    pub private: RoadPrivate,
    pub meta: OsmMeta,
    pub geom: LineString<f64>,
    pub minzoom: i32,
}
