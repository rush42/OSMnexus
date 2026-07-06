use std::collections::HashSet;

use serde_json::{Map, Value};

use crate::classify::categories::{categorize, eval_filter, resolve_minzoom, CategoryContext};
use crate::engine::extract::ExtractCtx;
use crate::engine::topic::Field;
use crate::engine::topic_runner::TopicRunner;
use crate::osm::types::{ElementKind, OsmWay, RawTags};
use crate::output::{
    geometry::{haversine_length_m, point_to_ewkb, to_ewkb, wgs84_to_3857},
    rows::{GeomRow, NodeGeomRow, TopicRow, WayGeomRow},
    types::{OsmMeta, Side},
};
use crate::transform::side_split::get_transformed_objects;

// ── Field evaluation ────────────────────────────────────────────────────────────

/// Evaluate each `Field`'s producer against `ctx`, inserting non-empty results into `map`.
/// When a value carries provenance, also emit `<output>_source` / `<output>_confidence`.
/// Each produced output key is recorded in `written` so the caller can tell which const
/// defaults were overwritten (used to gate bundled-const companion emission).
/// Used for `osm_fields`, sanitizers, and derivers alike.
fn eval_fields(
    fields: &[Field],
    ctx: &ExtractCtx,
    map: &mut Map<String, Value>,
    written: &mut HashSet<String>,
) {
    for field in fields {
        if let Some(p) = field.source.eval(ctx) {
            map.insert(field.output.clone(), p.value);
            written.insert(field.output.clone());
            // Companion consts → `<output>_<k>` (e.g. surface_source, smoothness_confidence).
            for (k, v) in p.consts {
                map.insert(format!("{}_{}", field.output, k), v);
            }
        }
    }
}

/// Interpret a category/topic const entry. A JSON object carrying a `value` field is a *bundled*
/// const: its `value` is the const itself, and its optional `consts` map holds companions emitted
/// as `<key>_<companion>` — but only when the const "wins" (no sanitizer/deriver produced `key`),
/// mirroring the branch-const provenance rule. Any other JSON is a bare literal with no companions.
fn const_entry(v: &Value) -> (&Value, Option<&Map<String, Value>>) {
    if let Value::Object(obj) = v {
        if let Some(value) = obj.get("value") {
            return (value, obj.get("consts").and_then(Value::as_object));
        }
    }
    (v, None)
}

// ── Public entry point ────────────────────────────────────────────────────────

pub fn build_topic_rows(
    runner: &TopicRunner,
    kind: ElementKind,
    osm_id: i64,
    tags: RawTags,
    meta: &OsmMeta,
) -> Vec<TopicRow> {
    let topic = &runner.spec;
    // The category set for this element kind. Absent → the topic has no categories for this kind.
    let categories = match runner.categories.get(&kind) {
        Some(c) => c,
        None => return Vec::new(),
    };

    // Evaluate optional way-level exclude condition before any categorization.
    if let Some(cond) = &topic.exclude_condition {
        if eval_filter(cond, &tags, &categories.macros, &runner.sanitizers) {
            return Vec::new();
        }
    }

    // Side-split (center-line) transforms are way-oriented; nodes/relations are never side-split.
    let no_transforms = Vec::new();
    let transformations = if kind == ElementKind::Way { &runner.transformations } else { &no_transforms };
    // Moves `tags` into the self object rather than cloning it (the common no-side-split case).
    let transformed = get_transformed_objects(tags, transformations);
    let mut rows = Vec::new();

    for obj in &transformed {
        // Side objects read their parent (the self object, always index 0) for parent-highway
        // tags; the self object owns the way's tags now that they're moved rather than cloned.
        let parent_tags: Option<&RawTags> =
            obj.parent_highway.as_ref().map(|_| &transformed[0].tags);
        let ctx = CategoryContext {
            tags: &obj.tags,
            side: obj.side,
            prefix: obj.prefix,
            parent_highway: obj.parent_highway.as_deref(),
            parent_tags,
            infix: obj.infix,
            sanitizers: &runner.sanitizers,
        };

        let category = match crate::profile::time(&crate::profile::CATEGORIZE, || categorize(&ctx, categories)) {
            Some(c) => c,
            None => continue,
        };
        // Times the rest of this iteration (field eval + const seeding + row build).
        let _extract = crate::profile::scope(&crate::profile::EXTRACT);

        let side_str = match obj.side {
            Side::Left  => "left",
            Side::Right => "right",
            Side::Self_ => "self",
        };

        let ectx = ExtractCtx {
            obj_tags: &obj.tags,
            parent_tags,
            parking_inference: category.parking_inference.as_deref(),
            obj_side: side_str,
            sanitizers: &runner.sanitizers,
            derivers: &runner.deriver_lib,
        };

        let mut osm = Map::new();
        let mut osm_written = HashSet::new();
        eval_fields(&topic.osm_fields, &ectx, &mut osm, &mut osm_written);

        // Sanitizer + deriver outputs share one column. Sanitizers apply to every category;
        // derivers come from this category's effective set (topic defaults ± overrides).
        let derivers = runner
            .category_derivers
            .get(&category.id)
            .unwrap_or(&runner.topic_derivers);
        let mut derived = Map::new();
        let mut private = Map::new();
        // Lowest-priority layer: seed category const *values* (bundled entries contribute only
        // their `value` here). Any bundled companions are emitted after field evaluation, and
        // only for keys no sanitizer/deriver overwrote ("the const wins").
        let consts = runner.category_consts.get(&category.id);
        if let Some(consts) = consts {
            for (k, v) in consts {
                let (value, _) = const_entry(v);
                derived.insert(k.clone(), value.clone());
            }
        }
        if let Some(privates) = runner.category_private.get(&category.id) {
            for (k, v) in privates {
                private.insert(k.clone(), v.clone());
            }
        }
        let mut written = HashSet::new();
        eval_fields(&runner.sanitizer_fields, &ectx, &mut derived, &mut written);
        eval_fields(derivers, &ectx, &mut derived, &mut written);

        // Emit bundled-const companions for entries still holding their const default (not
        // produced by a sanitizer/deriver): `<key>_<companion>` into `derived`, mirroring the
        // branch-const provenance rule (e.g. oneway that fell through to the implicit default
        // contributes `oneway_confidence`).
        if let Some(consts) = consts {
            for (k, v) in consts {
                if written.contains(k) {
                    continue;
                }
                if let (_, Some(companions)) = const_entry(v) {
                    for (ck, cv) in companions {
                        derived.insert(format!("{k}_{ck}"), cv.clone());
                    }
                }
            }
        }

        derived.insert("category".into(), Value::String(category.id.clone()));

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

        // Category override wins over the topic-level default; absent → 0.
        let minzoom = category
            .minzoom
            .as_ref()
            .or(topic.minzoom.as_ref())
            .map(|rule| resolve_minzoom(rule, &ctx, &categories.macros))
            .unwrap_or(0);

        // One tag row per transformed object; geometry (and its per-segment length) lives in the
        // geom table (see `build_geom_rows`), joined on `osm_id` at materialization time.
        let prefix = kind.id_prefix();
        let id = match obj.side {
            Side::Self_ => format!("{}/{}", prefix, osm_id),
            Side::Left  => format!("{}/{}/{}/left",  prefix, osm_id, obj.prefix.unwrap_or("cycleway")),
            Side::Right => format!("{}/{}/{}/right", prefix, osm_id, obj.prefix.unwrap_or("cycleway")),
        };
        derived.insert("id".into(), Value::String(id.clone()));

        rows.push(TopicRow {
            osm_id,
            osm_type: kind.osm_type(),
            id,
            osm,
            derived,
            private,
            meta: meta.clone(),
            minzoom,
        });
    }

    rows
}

/// Build the graph-edge rows for a way: one `GeomRow` per consecutive `cut_points` pair (the
/// sub-linestring `geom.0[s..=e]`, inclusive so the shared node joins both neighbours), with
/// per-segment `start_id`/`end_id`/`length_m`. A way with no interior cut-points yields a single
/// edge spanning the whole way. Topic-independent (same for every topic and every side object), so
/// computed once per way and written to the shared `edges` table.
pub fn build_geom_rows(way: &OsmWay, geom: &geo::LineString<f64>, length_m: f64) -> Vec<GeomRow> {
    let first_node = way.cut_points.first().map(|c| c.1).unwrap_or(0);
    let last_node = way.cut_points.last().map(|c| c.1).unwrap_or(0);

    let mut rows = Vec::new();

    if way.cut_points.len() > 2 {
        for (i, w) in way.cut_points.windows(2).enumerate() {
            let (s, e) = (w[0].0 as usize, w[1].0 as usize);
            let seg_line = geo::LineString::new(geom.0[s..=e].to_vec());
            let seg_len = haversine_length_m(&way.coords[s..=e]);
            rows.push(GeomRow {
                osm_id: way.id,
                seg_idx: i,
                start_id: w[0].1,
                end_id: w[1].1,
                geom_ewkb: to_ewkb(&seg_line),
                length_m: seg_len,
                total_length_m: length_m,
            });
        }
    } else {
        // No interior intersections: the whole way is a single edge.
        rows.push(GeomRow {
            osm_id: way.id,
            seg_idx: 0,
            start_id: first_node,
            end_id: last_node,
            geom_ewkb: to_ewkb(geom),
            length_m,
            total_length_m: length_m,
        });
    }

    rows
}

/// Build the whole-way linestring row for a way, emitted only with `--emit-way-geometries`.
pub fn build_way_geom_row(way: &OsmWay, geom: &geo::LineString<f64>, length_m: f64) -> WayGeomRow {
    WayGeomRow { osm_id: way.id, geom_ewkb: to_ewkb(geom), length_m }
}

/// Build the point-geometry row for a classified node, emitted only with `--emit-node-geometries`.
pub fn build_node_geom_row(id: i64, lon: f64, lat: f64) -> NodeGeomRow {
    let (x, y) = wgs84_to_3857(lon, lat);
    NodeGeomRow { osm_id: id, geom_ewkb: point_to_ewkb(x, y) }
}
