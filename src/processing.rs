use rustc_hash::FxHashMap;

use crate::topic::geom::{build_geom_rows, build_way_geom_row};
use crate::topic::TopicRunner;
use crate::osm::types::{ElementKind, NodeData, OsmWay, RelData, WayData, WayMeta};
use crate::output::{
    geometry::{haversine_length_m, project_line},
    rows::{GeomRow, TopicRow, WayGeomRow},
    types::OsmMeta,
};

/// Build the `OsmMeta` (updated_at/by, changeset) from an element's raw metadata. Shared by the
/// way/relation/node classification entry points.
fn meta_from(m: &WayMeta) -> OsmMeta {
    OsmMeta {
        updated_at: m.timestamp.and_then(|ts| {
            chrono::DateTime::from_timestamp(ts, 0)
                .map(|dt: chrono::DateTime<chrono::Utc>| dt.format("%Y-%m-%dT%H:%M:%SZ").to_string())
        }),
        updated_by: m.user.clone(),
        changeset_id: m.changeset,
    }
}

/// Run every topic's pipeline for one element of `kind` against its tags, returning per-topic tag
/// rows (index matches the runners slice). No geometry — classification is tag-only.
fn classify_element(
    runners: &[TopicRunner],
    kind: ElementKind,
    id: i64,
    tags: &crate::osm::types::RawTags,
    meta: &OsmMeta,
) -> Vec<Vec<TopicRow>> {
    runners.iter().map(|r| r.process(kind, id, tags, meta)).collect()
}

/// Tag rows for one way: per-topic tag rows plus a bitmask of which topics kept the way (produced
/// ≥1 row). The mask lets the caller decide whether the way is kept for geometry.
pub struct ClassifyOutput {
    pub topic_rows: Vec<Vec<TopicRow>>,
    pub mask: u32,
}

/// Tag-only classification for one way (Pass A). Runs every topic's way pipeline against the way's
/// raw tags. No geometry — coords are not needed and not available yet.
pub fn classify_way(runners: &[TopicRunner], wd: &WayData) -> ClassifyOutput {
    let meta = meta_from(&wd.meta);
    let mut mask = 0u32;
    let topic_rows: Vec<Vec<TopicRow>> = runners
        .iter()
        .enumerate()
        .map(|(i, r)| {
            let rows = r.process(ElementKind::Way, wd.id, &wd.tags, &meta);
            if !rows.is_empty() {
                mask |= 1 << i;
            }
            rows
        })
        .collect();
    ClassifyOutput { topic_rows, mask }
}

/// Tag-only classification for one relation (relations pass). Per-topic relation tag rows.
pub fn classify_relation(runners: &[TopicRunner], rd: &RelData) -> Vec<Vec<TopicRow>> {
    let meta = meta_from(&rd.meta);
    classify_element(runners, ElementKind::Relation, rd.id, &rd.tags, &meta)
}

/// Tag-only classification for one node (nodes pass). Per-topic node tag rows.
pub fn classify_node(runners: &[TopicRunner], nd: &NodeData) -> Vec<Vec<TopicRow>> {
    let meta = meta_from(&nd.meta);
    classify_element(runners, ElementKind::Node, nd.id, &nd.tags, &meta)
}

/// Build the graph-edge rows for a resolved way (geometry pass): always emitted, one row per
/// intersection segment. Projects the line + measures length once. Topic-independent. `node_ids` is
/// the `osm node id -> internal id` map from `assign_node_ids`, used to resolve `start_id`/`end_id`.
pub fn geom_rows_for(way: &OsmWay, node_ids: &FxHashMap<i64, i64>) -> Vec<GeomRow> {
    let _t = crate::profiling::time(&crate::profiling::GEOMETRY);
    let length_m = haversine_length_m(&way.coords);
    let geom = project_line(&way.coords);
    build_geom_rows(way, &geom, length_m, node_ids)
}

/// Build the whole-way linestring row for a resolved way. Only called when some topic declared
/// `"geometry": { "way": ["linestring"] }` (see `main.rs`'s `build_geom_cb`).
pub fn way_geom_row_for(way: &OsmWay) -> WayGeomRow {
    let _t = crate::profiling::time(&crate::profiling::GEOMETRY);
    let length_m = haversine_length_m(&way.coords);
    let geom = project_line(&way.coords);
    build_way_geom_row(way, &geom, length_m)
}
