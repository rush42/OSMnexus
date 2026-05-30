use super::types::{BikelaneDerived, BikelaneOsmTags, OsmMeta};
use geo::LineString;

pub struct BikelaneRow {
    pub osm_id: i64,
    pub osm_type: &'static str,
    pub id: String,
    pub osm: BikelaneOsmTags,
    pub derived: BikelaneDerived,
    pub meta: OsmMeta,
    pub geom: LineString<f64>,
    pub minzoom: i32,
}
