use serde_json::{Map, Value};

use crate::classify::{
    categories::{categorize, eval_filter, resolve_minzoom, CategoriesFile, CategoryContext},
    derive,
    road_classification::road_classification_value,
};
use crate::engine::extract::ExtractCtx;
use crate::engine::topic::{OsmFieldSpec, SanitizedField, TopicSpec};
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

fn extract_osm(fields: &[OsmFieldSpec], ctx: &ExtractCtx) -> Map<String, Value> {
    let mut map = Map::new();
    for spec in fields {
        if let Some(v) = spec.source.eval(ctx) {
            map.insert(spec.output.clone(), v);
        }
    }
    map
}

// ── Field production (sanitizers + single/two-output derivers) ──────────────────

fn build_fields(
    fields: &[SanitizedField],
    ctx: &ExtractCtx,
    category_id: &str,
    side_str: &str,
) -> Map<String, Value> {
    let mut map = Map::new();

    for field in fields {
        match field {
            SanitizedField::Produce { output, source } => {
                if let Some(v) = source.eval(ctx) {
                    map.insert(output.clone(), v);
                }
            }
            // Two-output deriver: explicit traffic_mode (left/right) with parking inference.
            SanitizedField::TrafficMode { output_left, output_right, .. } => {
                let (tm_l, tm_r) = derive::derive_traffic_mode(
                    ctx.obj_tags, ctx.centerline_tags, category_id, side_str,
                );
                if let Some(v) = tm_l { map.insert(output_left.clone(),  Value::String(v)); }
                if let Some(v) = tm_r { map.insert(output_right.clone(), Value::String(v)); }
            }
        }
    }

    map
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

        let ectx = ExtractCtx {
            obj_tags: &obj.tags,
            parent_tags,
            centerline_tags: tags, // parent way tags for parking inference + lifecycle/surface fallback
            implicit_oneway: category.implicit_oneway,
            copy_surface_from_parent: category.copy_surface_smoothness_from_parent,
        };

        let osm = extract_osm(&topic.osm_fields, &ectx);

        // Sanitizer + deriver outputs share one column. Start from the produced fields,
        // then add the inline derived values.
        let mut derived = build_fields(&topic.sanitized_fields, &ectx, category.id.as_str(), side_str);

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
