use crate::classify::{
    exclude::{by_access_bikelanes, by_service, should_exclude},
    minzoom::road_minzoom,
    road_classification::road_classification_value,
    sanitize::{self as san},
};
use crate::engine::{runner::{build_topic_rows, TopicRow}, topic::TopicSpec};
use crate::osm::types::OsmWay;
use crate::output::{
    geometry::{haversine_length_m, project_line},
    road_row::RoadRow,
    types::{OsmMeta, RawTagsRef, RoadDerived, RoadOsm, RoadSanitized},
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
    pub road_row: Option<RoadRow>,
}

pub fn process_way(
    way: &OsmWay,
    transformations: &[CenterLineTransformation],
    topic: &TopicSpec,
) -> WayOutput {
    let mut tags = way.tags.clone();

    transform_lifecycle_tags(&mut tags);
    transform_cycleway_opposite_schema(&mut tags);
    transform_construction_prefix(&mut tags);
    transform_cycleway_both_postfix(&mut tags);

    if should_exclude(&tags) {
        return WayOutput { bikelane_rows: Vec::new(), road_row: None };
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

    let road_row = build_road_row(way, &tags, &geom, length_m, &meta);
    let bikelane_rows = build_topic_rows(topic, way, &tags, transformations, &geom, length_m, &meta);

    WayOutput { bikelane_rows, road_row }
}

fn raw(tags: &RawTagsRef, key: &str) -> Option<String> {
    tags.get(key).cloned()
}

fn build_road_row(
    way: &OsmWay,
    tags: &RawTagsRef,
    geom: &geo::LineString<f64>,
    length_m: f64,
    meta: &OsmMeta,
) -> Option<RoadRow> {
    if by_access_bikelanes(tags) || by_service(tags) { return None; }
    let road = road_classification_value(tags)?;
    let highway = tags.get("highway").cloned().unwrap_or_default();
    let id = format!("way/{}", way.id);
    let minzoom = road_minzoom(&road);

    Some(RoadRow {
        osm_id: way.id,
        osm_type: "W",
        id: id.clone(),
        osm: RoadOsm {
            highway,
            name:           raw(tags, "name"),
            name_ref:       raw(tags, "ref"),
            surface:        raw(tags, "surface"),
            smoothness:     raw(tags, "smoothness"),
            maxspeed:       raw(tags, "maxspeed"),
            oneway:         raw(tags, "oneway"),
            oneway_bicycle: raw(tags, "oneway:bicycle"),
            lit:            raw(tags, "lit"),
            bridge:         raw(tags, "bridge"),
            tunnel:         raw(tags, "tunnel"),
            operator_type:  raw(tags, "operator_type"),
            informal:       raw(tags, "informal"),
            covered:        raw(tags, "covered"),
            traffic_sign:   raw(tags, "traffic_sign"),
        },
        sanitized: RoadSanitized {
            bridge:       san::sanitize_yes_flag(tags, "bridge"),
            tunnel:       san::sanitize_yes_flag(tags, "tunnel"),
            traffic_sign: raw(tags, "traffic_sign").as_deref().and_then(san::sanitize_traffic_sign),
        },
        derived: RoadDerived {
            id,
            road,
            length_m,
            lifecycle: raw(tags, "lifecycle"),
            bikelane_left: None,
            bikelane_right: None,
            bikelane_self: None,
        },
        meta: meta.clone(),
        geom: geom.clone(),
        minzoom,
    })
}
