use crate::config::SplitMode;
use crate::engine::runner::{build_geom_rows, GeomRow, TopicRow};
use crate::engine::topic_runner::TopicRunner;
use crate::osm::types::OsmWay;
use crate::output::{
    geometry::{haversine_length_m, project_line},
    types::OsmMeta,
};
use crate::profile::{self, CLASSIFY, GEOMETRY};

/// Output of processing one way: per-topic tag rows (index matches the runners slice) plus the
/// shared geometry rows (topic-independent — the same line for every topic and side object).
pub struct WayOutput {
    pub topic_rows: Vec<Vec<TopicRow>>,
    pub geom_rows: Vec<GeomRow>,
}

/// Thin dispatcher: compute the shared geometry/length/metadata once, then classify the way's raw
/// tags for every topic (tag-only) and build the geometry rows once. The tag rows and geometry
/// rows are written to separate tables and joined at materialization time on `osm_id`.
pub fn process_way(way: &OsmWay, runners: &[TopicRunner], split: SplitMode) -> WayOutput {
    let (length_m, geom) = profile::time(&GEOMETRY, || {
        (haversine_length_m(&way.coords), project_line(&way.coords))
    });

    let meta = OsmMeta {
        updated_at: way.meta.timestamp.and_then(|ts| {
            chrono::DateTime::from_timestamp(ts, 0)
                .map(|dt: chrono::DateTime<chrono::Utc>| dt.format("%Y-%m-%dT%H:%M:%SZ").to_string())
        }),
        updated_by: way.meta.user.clone(),
        changeset_id: way.meta.changeset,
    };

    let topic_rows: Vec<Vec<TopicRow>> = profile::time(&CLASSIFY, || {
        runners
            .iter()
            .map(|r| r.process(way, &way.tags, &meta))
            .collect()
    });

    let geom_rows = profile::time(&GEOMETRY, || build_geom_rows(way, &geom, length_m, split));

    WayOutput { topic_rows, geom_rows }
}
