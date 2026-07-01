use crate::engine::{runner::TopicRow, topic_runner::TopicRunner};
use crate::osm::types::OsmWay;
use crate::output::{
    geometry::{haversine_length_m, project_line},
    types::OsmMeta,
};
use crate::profile::{self, CLASSIFY, GEOMETRY};

/// Rows per topic (index matches the runners slice passed to process_way).
pub struct WayOutput(pub Vec<Vec<TopicRow>>);

/// Thin dispatcher: compute the shared geometry/length/metadata once, then hand the way's
/// raw tags to every topic. Each topic owns its full pipeline (transforms → exclude →
/// categorize → extract), all declared in its `topic.json`.
pub fn process_way(way: &OsmWay, runners: &[TopicRunner]) -> WayOutput {
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

    let rows = profile::time(&CLASSIFY, || {
        runners
            .iter()
            .map(|r| r.process(way, &way.tags, &geom, length_m, &meta))
            .collect()
    });

    WayOutput(rows)
}
