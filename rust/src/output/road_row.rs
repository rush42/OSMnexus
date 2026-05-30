use super::types::{OsmMeta, RoadDerived, RoadOsmTags};
use geo::LineString;

pub struct RoadRow {
    pub osm_id: i64,
    pub osm_type: &'static str,
    pub id: String,
    pub osm: RoadOsmTags,
    pub derived: RoadDerived,
    pub meta: OsmMeta,
    pub geom: LineString<f64>,
    pub minzoom: i32,
}
