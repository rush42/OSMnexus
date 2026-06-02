use serde_json::{Map, Value};

use crate::classify::{
    categories::{categorize, eval_filter, resolve_minzoom, CategoriesFile, CategoryContext},
    derive,
    road_classification::road_classification_value,
    sanitize as san,
};
use crate::engine::topic::{OsmFieldSpec, SanitizerSpec, TagSource, TopicSpec};
use crate::osm::types::{OsmWay, RawTags};
use crate::output::{
    geometry::to_ewkb,
    types::{OsmMeta, Side},
};
use crate::transform::side_split::{get_transformed_objects, CenterLineTransformation};

/// A single output row produced by the topic engine.
/// All four data columns are runtime JSON maps — no per-topic typed structs needed.
pub struct TopicRow {
    pub osm_id: i64,
    pub osm_type: &'static str,
    pub id: String,
    pub osm: Map<String, Value>,
    /// Merged sanitizer + deriver outputs (the former `sanitized` ∪ `derived` columns).
    pub derived: Map<String, Value>,
    pub private: Map<String, Value>,
    pub meta: OsmMeta,
    pub geom_ewkb: Vec<u8>,
    pub minzoom: i32,
}

impl TopicRow {
    pub fn to_csv_fields(&self) -> anyhow::Result<[String; 9]> {
        Ok([
            self.osm_id.to_string(),
            self.osm_type.to_owned(),
            self.id.clone(),
            serde_json::to_string(&self.osm)?,
            serde_json::to_string(&self.derived)?,
            serde_json::to_string(&self.private)?,
            serde_json::to_string(&self.meta)?,
            hex::encode(&self.geom_ewkb),
            self.minzoom.to_string(),
        ])
    }
}

// ── OSM field extraction ──────────────────────────────────────────────────────

fn extract_osm(
    fields: &[OsmFieldSpec],
    obj_tags: &RawTags,
    parent_tags: Option<&RawTags>,
) -> Map<String, Value> {
    let mut map = Map::new();
    for spec in fields {
        let value = match spec.source {
            TagSource::Obj => first_of(spec.keys.as_slice(), obj_tags),
            TagSource::Parent => parent_tags.and_then(|pt| first_of(spec.keys.as_slice(), pt)),
            TagSource::ObjThenParent => first_of(spec.keys.as_slice(), obj_tags)
                .or_else(|| parent_tags.and_then(|pt| first_of(spec.keys.as_slice(), pt))),
        };
        if let Some(v) = value {
            map.insert(spec.output.clone(), Value::String(v));
        }
    }
    map
}

fn first_of(keys: &[String], tags: &RawTags) -> Option<String> {
    keys.iter().find_map(|k| tags.get(k).cloned())
}

// ── Sanitizer dispatch ────────────────────────────────────────────────────────

fn apply_sanitizers(
    specs: &[SanitizerSpec],
    obj_tags: &RawTags,
    parent_tags: Option<&RawTags>,
    centerline_tags: &RawTags,
    category_id: &str,
    implicit_oneway: bool,
    copy_surface_from_parent: bool,
    side_str: &str,
) -> Map<String, Value> {
    let mut map = Map::new();

    for spec in specs {
        match spec {
            SanitizerSpec::TrafficSign { output } => {
                if let Some(v) = obj_tags.get("traffic_sign").and_then(|r| san::sanitize_traffic_sign(r)) {
                    map.insert(output.clone(), Value::String(v));
                }
            }
            SanitizerSpec::Separation { output, side } => {
                if let Some(v) = san::separation(obj_tags, side) {
                    map.insert(output.clone(), Value::String(v));
                }
            }
            SanitizerSpec::Marking { output, side } => {
                if let Some(v) = san::marking(obj_tags, side) {
                    map.insert(output.clone(), Value::String(v));
                }
            }
            SanitizerSpec::Buffer { output, side } => {
                if let Some(v) = san::buffer(obj_tags, side) {
                    map.insert(output.clone(), Value::Number(float_to_json(v)));
                }
            }
            SanitizerSpec::SurfaceColor { output } => {
                if let Some(v) = san::surface_color(obj_tags) {
                    map.insert(output.clone(), Value::String(v));
                }
            }
            SanitizerSpec::YesFlag { output, key, source } => {
                let tags = match source {
                    TagSource::Parent => parent_tags.unwrap_or(obj_tags),
                    _ => obj_tags,
                };
                if san::sanitize_yes_flag(tags, key).is_some() {
                    map.insert(output.clone(), Value::Bool(true));
                }
            }
            SanitizerSpec::ParseLength { output, key } => {
                if let Some(v) = obj_tags.get(key).and_then(|r| san::parse_length(r)) {
                    map.insert(output.clone(), Value::Number(float_to_json(v)));
                }
            }
            SanitizerSpec::Lifecycle { output } => {
                let v = san::temporary(obj_tags)
                    .map(str::to_owned)
                    .or_else(|| obj_tags.get("lifecycle").cloned())
                    .or_else(|| centerline_tags.get("lifecycle").cloned());
                if let Some(v) = v {
                    map.insert(output.clone(), Value::String(v));
                }
            }
            SanitizerSpec::SurfaceWithFallback { output } => {
                let v = obj_tags.get("surface")
                    .or_else(|| if copy_surface_from_parent { centerline_tags.get("surface") } else { None })
                    .cloned();
                if let Some(v) = v {
                    map.insert(output.clone(), Value::String(v));
                }
            }
            SanitizerSpec::SmoothnessWithFallback { output } => {
                let v = obj_tags.get("smoothness")
                    .or_else(|| if copy_surface_from_parent { centerline_tags.get("smoothness") } else { None })
                    .cloned();
                if let Some(v) = v {
                    map.insert(output.clone(), Value::String(v));
                }
            }
            SanitizerSpec::DeriveOneway { output } => {
                map.insert(output.clone(), Value::String(derive::derive_oneway(obj_tags, implicit_oneway)));
            }
            SanitizerSpec::DeriveTrafficMode { output_left, output_right } => {
                let (tm_l, tm_r) = derive::derive_traffic_mode(obj_tags, centerline_tags, category_id, side_str);
                if let Some(v) = tm_l { map.insert(output_left.clone(),  Value::String(v)); }
                if let Some(v) = tm_r { map.insert(output_right.clone(), Value::String(v)); }
            }
        }
    }

    map
}

fn float_to_json(v: f32) -> serde_json::Number {
    serde_json::Number::from_f64(v as f64)
        .unwrap_or_else(|| serde_json::Number::from(0))
}

// ── Public entry point ────────────────────────────────────────────────────────

pub fn build_topic_rows(
    topic: &TopicSpec,
    categories: &CategoriesFile,
    way: &OsmWay,
    tags: &RawTags,
    transformations: &[CenterLineTransformation],
    geom: &geo::LineString<f64>,
    length_m: f64,
    meta: &OsmMeta,
) -> Vec<TopicRow> {
    // Evaluate optional way-level exclude condition before any categorization.
    if let Some(cond) = &topic.exclude_condition {
        if eval_filter(cond, tags, &categories.macros) {
            return Vec::new();
        }
    }

    let transformed = get_transformed_objects(tags, transformations);
    let mut rows = Vec::new();

    for obj in &transformed {
        let parent_tags: Option<&RawTags> = obj.parent_highway.as_ref().map(|_| tags);
        let ctx = CategoryContext {
            tags: &obj.tags,
            side: obj.side,
            prefix: obj.prefix,
            parent_highway: obj.parent_highway.as_deref(),
            parent_tags,
            infix: obj.infix,
            length_m,
        };

        let Some(category) = categorize(&ctx, categories) else { continue };
        if !category.infrastructure_exists { continue }

        let id = match obj.side {
            Side::Self_ => format!("way/{}", way.id),
            Side::Left  => format!("way/{}/{}/left",  way.id, obj.prefix.unwrap_or("cycleway")),
            Side::Right => format!("way/{}/{}/right", way.id, obj.prefix.unwrap_or("cycleway")),
        };

        let side_str = match obj.side {
            Side::Left  => "left",
            Side::Right => "right",
            Side::Self_ => "self",
        };

        let osm = extract_osm(&topic.osm_fields, &obj.tags, parent_tags);

        // Sanitizer + deriver outputs share one column. Start from the sanitizer outputs,
        // then add the derived values.
        let mut derived = apply_sanitizers(
            &topic.sanitized_fields,
            &obj.tags,
            parent_tags,
            tags, // centerline (parent way) tags for parking inference + lifecycle fallback
            category.id.as_str(),
            category.implicit_oneway,
            category.copy_surface_smoothness_from_parent,
            side_str,
        );

        derived.insert("id".into(),       Value::String(id.clone()));
        derived.insert("category".into(), Value::String(category.id.clone()));
        derived.insert("length_m".into(), Value::Number(serde_json::Number::from_f64(length_m).unwrap()));
        if let Some(road) = road_classification_value(tags) {
            derived.insert("road".into(), Value::String(road));
        }

        let mut private = Map::new();
        private.insert("_side".into(), Value::String(side_str.to_owned()));
        if let Some(p) = obj.prefix {
            private.insert("_prefix".into(), Value::String(p.to_owned()));
        }
        if let Some(i) = obj.infix {
            private.insert("_infix".into(), Value::String(i.to_owned()));
        }
        if let Some(ph) = &obj.parent_highway {
            private.insert("_parent_highway".into(), Value::String(ph.clone()));
        }
        private.insert(
            "_implicit_oneway_confidence".into(),
            Value::String(category.implicit_oneway_confidence.clone()),
        );

        // Category override wins over the topic-level default; absent → 0.
        let minzoom = category
            .minzoom
            .as_ref()
            .or(topic.minzoom.as_ref())
            .map(|rule| resolve_minzoom(rule, &ctx, &categories.macros))
            .unwrap_or(0);

        rows.push(TopicRow {
            osm_id: way.id,
            osm_type: "W",
            id,
            osm,
            derived,
            private,
            meta: meta.clone(),
            geom_ewkb: to_ewkb(geom),
            minzoom,
        });
    }

    rows
}
