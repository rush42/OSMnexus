use crate::classify::exclude::should_exclude;
use crate::engine::{runner::TopicRow, topic_runner::TopicRunner};
use crate::osm::types::OsmWay;
use crate::output::{
    geometry::{haversine_length_m, project_line},
    types::OsmMeta,
};
use crate::transform::{
    construction_prefix::transform_construction_prefix,
    cycleway_both::transform_cycleway_both_postfix,
    lifecycle::transform_lifecycle_tags,
    opposite::transform_cycleway_opposite_schema,
};

/// Rows per topic (index matches the runners slice passed to process_way).
pub struct WayOutput(pub Vec<Vec<TopicRow>>);

pub fn process_way(way: &OsmWay, runners: &[TopicRunner]) -> WayOutput {
    let mut tags = way.tags.clone();

    transform_lifecycle_tags(&mut tags);
    transform_cycleway_opposite_schema(&mut tags);
    transform_construction_prefix(&mut tags);
    transform_cycleway_both_postfix(&mut tags);

    if should_exclude(&tags) {
        return WayOutput(runners.iter().map(|_| Vec::new()).collect());
    }

    let length_m = haversine_length_m(&way.coords);
    let geom = project_line(&way.coords);

    let meta = OsmMeta {
        updated_at: way.meta.timestamp.and_then(|ts| {
            chrono::DateTime::from_timestamp(ts, 0)
                .map(|dt: chrono::DateTime<chrono::Utc>| dt.format("%Y-%m-%dT%H:%M:%SZ").to_string())
        }),
        updated_by: way.meta.user.clone(),
        changeset_id: way.meta.changeset,
    };

    let rows = runners
        .iter()
        .map(|r| r.process(way, &tags, &geom, length_m, &meta))
        .collect();

    WayOutput(rows)
}
