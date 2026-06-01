use crate::classify::bikelane_categories::CategoriesFile;
use crate::classify::exclude::should_exclude;
use crate::engine::{runner::{build_topic_rows, TopicRow}, topic::TopicSpec};
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
    side_split::CenterLineTransformation,
};

pub struct WayOutput {
    pub bikelane_rows: Vec<TopicRow>,
    pub road_rows: Vec<TopicRow>,
}

pub fn process_way(
    way: &OsmWay,
    bikelane_transformations: &[CenterLineTransformation],
    bikelane_topic: &TopicSpec,
    bikelane_categories: &CategoriesFile,
    road_topic: &TopicSpec,
    road_categories: &CategoriesFile,
) -> WayOutput {
    let mut tags = way.tags.clone();

    transform_lifecycle_tags(&mut tags);
    transform_cycleway_opposite_schema(&mut tags);
    transform_construction_prefix(&mut tags);
    transform_cycleway_both_postfix(&mut tags);

    if should_exclude(&tags) {
        return WayOutput { bikelane_rows: Vec::new(), road_rows: Vec::new() };
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

    let bikelane_rows = build_topic_rows(
        bikelane_topic, bikelane_categories,
        way, &tags, bikelane_transformations, &geom, length_m, &meta,
    );

    // Road topic uses no transformations (empty slice) — always processes as self object
    let road_rows = build_topic_rows(
        road_topic, road_categories,
        way, &tags, &[], &geom, length_m, &meta,
    );

    WayOutput { bikelane_rows, road_rows }
}
