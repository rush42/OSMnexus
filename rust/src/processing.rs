use crate::classify::{
    bikelane_categories::{categorize_bikelane, CategoryContext},
    exclude::{by_access_bikelanes, by_service, should_exclude},
    minzoom::{bikelane_minzoom, road_minzoom},
    road_classification::road_classification_value,
    sanitize::{self as san},
};
use crate::osm::types::OsmWay;
use crate::output::{
    bikelane_row::BikelaneRow,
    geometry::{haversine_length_m, project_line},
    road_row::RoadRow,
    types::{
        BikelaneDerived, BikelaneOsm, BikelanePrivate, BikelaneSanitized,
        OsmMeta, RawTagsRef, RoadDerived, RoadOsm, RoadSanitized, Side,
    },
};
use crate::transform::{
    construction_prefix::transform_construction_prefix,
    cycleway_both::transform_cycleway_both_postfix,
    lifecycle::transform_lifecycle_tags,
    opposite::transform_cycleway_opposite_schema,
    side_split::{get_transformed_objects, CenterLineTransformation},
};

pub struct WayOutput {
    pub bikelane_rows: Vec<BikelaneRow>,
    pub road_row: Option<RoadRow>,
}

pub fn process_way(way: &OsmWay, transformations: &[CenterLineTransformation]) -> WayOutput {
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
    let bikelane_rows = build_bikelane_rows(way, &tags, transformations, &geom, length_m, &meta);

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

fn build_bikelane_rows(
    way: &OsmWay,
    tags: &RawTagsRef,
    transformations: &[CenterLineTransformation],
    geom: &geo::LineString<f64>,
    length_m: f64,
    meta: &OsmMeta,
) -> Vec<BikelaneRow> {
    let transformed = get_transformed_objects(tags, transformations);
    let mut rows = Vec::new();

    for obj in &transformed {
        let parent_tags = obj.parent_highway.as_ref().map(|_| tags);
        let ctx = CategoryContext {
            tags: &obj.tags,
            side: obj.side,
            prefix: obj.prefix,
            parent_highway: obj.parent_highway.as_deref(),
            parent_tags,
            infix: obj.infix,
            length_m,
        };

        let Some(category) = categorize_bikelane(&ctx) else { continue };
        if !category.infrastructure_exists { continue }

        let id = match obj.side {
            Side::Self_ => format!("way/{}", way.id),
            Side::Left  => format!("way/{}/{}/left",  way.id, obj.prefix.unwrap_or("cycleway")),
            Side::Right => format!("way/{}/{}/right", way.id, obj.prefix.unwrap_or("cycleway")),
        };

        let otags = &obj.tags;

        let copy_from_parent = category.copy_surface_smoothness_from_parent;
        let surface = raw(otags, "surface")
            .or_else(|| if copy_from_parent { raw(tags, "surface") } else { None });
        let smoothness = raw(otags, "smoothness")
            .or_else(|| if copy_from_parent { raw(tags, "smoothness") } else { None });

        let lifecycle = san::temporary(otags)
            .map(str::to_owned)
            .or_else(|| raw(otags, "lifecycle"))
            .or_else(|| raw(tags, "lifecycle"));

        let side_str = match obj.side {
            Side::Left  => "left",
            Side::Right => "right",
            Side::Self_ => "self",
        };
        let (tm_left, tm_right) =
            san::derive_traffic_mode(otags, tags, category.id.as_str(), side_str);

        rows.push(BikelaneRow {
            osm_id: way.id,
            osm_type: "W",
            id: id.clone(),
            osm: BikelaneOsm {
                name:             raw(otags, "name").or_else(|| raw(tags, "name")),
                surface:          raw(otags, "surface"),
                smoothness:       raw(otags, "smoothness"),
                width:            raw(otags, "width"),
                source_width:     raw(otags, "source:width"),
                bridge:           raw(tags, "bridge"),
                tunnel:           raw(tags, "tunnel"),
                oneway:           raw(otags, "oneway"),
                oneway_bicycle:   raw(otags, "oneway:bicycle").or_else(|| raw(tags, "oneway:bicycle")),
                traffic_sign:     raw(otags, "traffic_sign"),
                informal:         raw(tags, "informal"),
                covered:          raw(tags, "covered"),
                operator_type:    raw(tags, "operator_type"),
                mapillary:        raw(tags, "mapillary"),
                segregated:       raw(otags, "segregated"),
                bicycle:          raw(otags, "bicycle"),
                foot:             raw(otags, "foot"),
                description:      raw(otags, "description"),
                note:             raw(otags, "note"),
                temporary:        raw(otags, "temporary"),
                separation_left:  raw(otags, "separation:left"),
                separation_right: raw(otags, "separation:right"),
                separation_both:  raw(otags, "separation:both"),
                marking_left:     raw(otags, "marking:left"),
                marking_right:    raw(otags, "marking:right"),
                marking_both:     raw(otags, "marking:both"),
                traffic_mode_left:  raw(otags, "traffic_mode:left"),
                traffic_mode_right: raw(otags, "traffic_mode:right"),
                traffic_mode_both:  raw(otags, "traffic_mode:both"),
                buffer_left:      raw(otags, "buffer:left"),
                buffer_right:     raw(otags, "buffer:right"),
                buffer_both:      raw(otags, "buffer:both"),
                surface_colour:   raw(otags, "surface:colour").or_else(|| raw(otags, "surface:color")),
            },
            sanitized: BikelaneSanitized {
                traffic_sign:       raw(otags, "traffic_sign").as_deref().and_then(san::sanitize_traffic_sign),
                separation_left:    san::separation(otags, "left"),
                separation_right:   san::separation(otags, "right"),
                marking_left:       san::marking(otags, "left"),
                marking_right:      san::marking(otags, "right"),
                traffic_mode_left:  tm_left,
                traffic_mode_right: tm_right,
                buffer_left:        san::buffer(otags, "left"),
                buffer_right:       san::buffer(otags, "right"),
                surface_color:      san::surface_color(otags),
                bridge:             san::sanitize_yes_flag(tags, "bridge"),
                tunnel:             san::sanitize_yes_flag(tags, "tunnel"),
                oneway:             san::derive_oneway(otags, category.implicit_oneway),
                width:              raw(otags, "width").as_deref().and_then(san::parse_length),
                width_effective:    raw(otags, "width:effective").as_deref().and_then(san::parse_length),
                lifecycle,
                surface,
                smoothness,
            },
            derived: BikelaneDerived {
                id,
                category: category.id.as_str(),
                road: road_classification_value(tags),
                length_m,
            },
            private: BikelanePrivate {
                side:   obj.side,
                prefix: obj.prefix,
                infix:  obj.infix,
                parent_highway: obj.parent_highway.clone(),
                implicit_oneway_confidence: category.implicit_oneway_confidence.as_str(),
            },
            meta: meta.clone(),
            geom: geom.clone(),
            minzoom: bikelane_minzoom(length_m),
        });
    }
    rows
}
