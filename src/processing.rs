//! The "select" half of the select/materialize split: tag-only classification for one element at
//! a time (way/relation/node), each producing its per-topic tag rows plus (for ways) a keep mask.
//! Purely tag-driven — no coordinates, no geometry decisions; those live in `geom::materialize`,
//! called separately (from `main.rs`) once a way/relation is resolved.

use crate::topic::TopicRunner;
use crate::osm::types::{ElementKind, NodeData, RelData, WayData, WayMeta};
use crate::output::{rows::TopicRow, types::OsmMeta};

/// Build the `OsmMeta` (updated_at/by, changeset) from an element's raw metadata.
///
/// Called per emitted *row*, not per element scanned — the timestamp formatting alone measures
/// ~250 ns (chrono's strftime plus the `String` it allocates), which is ruinous on the nodes pass:
/// a category-based node topic walks every node in the file (3.8 billion on europe-latest) to keep
/// a handful, so formatting eagerly spent ~940 CPU-seconds there producing strings that were
/// discarded for all but ~27k of them. `build_topic_rows` therefore carries the borrowed `WayMeta`
/// through classification and calls this only once an element is actually being emitted.
pub(crate) fn meta_from(m: &WayMeta) -> OsmMeta {
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
    tags: &crate::osm::types::RawTags<'_>,
    meta: &WayMeta,
) -> Vec<Vec<TopicRow>> {
    runners.iter().map(|r| r.process(kind, id, tags, meta, &[])).collect()
}

/// Tag rows for one way: per-topic tag rows plus a bitmask of which topics kept the way (produced
/// ≥1 row). The mask lets the caller decide whether the way is kept for geometry.
pub struct ClassifyOutput {
    pub topic_rows: Vec<Vec<TopicRow>>,
    pub mask: u32,
}

/// Tag-only classification for one way (Pass A). Runs every topic's way pipeline against the way's
/// raw tags. No geometry — coords are not needed and not available yet.
/// `parents` is the way's parent relation ids (empty unless some topic declares
/// `inherit_to_member`); each topic resolves its own exported parent tagsets out of `inherit`, since
/// what a parent exports is per-topic. See `topic::inherit`.
pub fn classify_way(
    runners: &[TopicRunner],
    wd: &WayData<'_>,
    inherit: &crate::topic::inherit::InheritStore,
    parents: &[i64],
) -> ClassifyOutput {
    let mut mask = 0u32;
    let topic_rows: Vec<Vec<TopicRow>> = runners
        .iter()
        .enumerate()
        .map(|(i, r)| {
            let parents = inherit.parents_for(i, parents);
            let rows = r.process(ElementKind::Way, wd.id, &wd.tags, &wd.meta, &parents);
            if !rows.is_empty() {
                mask |= 1 << i;
            }
            rows
        })
        .collect();
    ClassifyOutput { topic_rows, mask }
}

/// Tag-only classification for one relation (relations pass). Per-topic relation tag rows.
pub fn classify_relation(runners: &[TopicRunner], rd: &RelData<'_>) -> Vec<Vec<TopicRow>> {
    classify_element(runners, ElementKind::Relation, rd.id, &rd.tags, &rd.meta)
}

/// Tag-only classification for one node (nodes pass). Per-topic node tag rows.
pub fn classify_node(runners: &[TopicRunner], nd: &NodeData<'_>) -> Vec<Vec<TopicRow>> {
    classify_element(runners, ElementKind::Node, nd.id, &nd.tags, &nd.meta)
}
