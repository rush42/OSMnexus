use std::collections::HashSet;

use serde_json::{Map, Value};

use crate::classify::categories::{categorize, eval_filter, resolve_minzoom, CategoryContext};
use crate::engine::extract::ExtractCtx;
use crate::engine::topic::Field;
use crate::engine::topic_runner::TopicRunner;
use crate::osm::types::{OsmWay, RawTags};
use crate::output::{
    geometry::{haversine_length_m, to_ewkb},
    types::{OsmMeta, Side},
};
use crate::transform::side_split::get_transformed_objects;

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
    way: &OsmWay,
    tags: RawTags,
    geom: &geo::LineString<f64>,
    length_m: f64,
    meta: &OsmMeta,
    split: bool,
) -> Vec<TopicRow> {
    let topic = &runner.spec;
    let categories = &runner.categories;

    // Evaluate optional way-level exclude condition before any categorization.
    if let Some(cond) = &topic.exclude_condition {
        if eval_filter(cond, &tags, &categories.macros, &runner.sanitizers) {
            return Vec::new();
        }
    }

    // Moves `tags` into the self object rather than cloning it (the common no-side-split case).
    let transformed = get_transformed_objects(tags, &runner.transformations);
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
            length_m,
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

        // `id`, `length_m`, and `total_length_m` are per-segment and set in the emission step below.
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

        // Optionally split the way at its intersection nodes: one row per sub-linestring, each
        // with its own geometry/`length_m` but sharing all tags. `total_length_m` always carries
        // the full way length. When not splitting (or no interior intersections) there is a single
        // segment and the maps are moved into the row — the hot path stays clone-free.
        let segments = segment_geoms(way, geom, length_m, split);

        // Insert the segment's `id` (before the side portion), `length_m`, and `total_length_m`.
        let finalize = |derived: &mut Map<String, Value>, seg_idx: Option<usize>, seg_len: f64| -> String {
            let seg = seg_idx.map(|i| format!("/{i}")).unwrap_or_default();
            let id = match obj.side {
                Side::Self_ => format!("way/{}{}", way.id, seg),
                Side::Left  => format!("way/{}{}/{}/left",  way.id, seg, obj.prefix.unwrap_or("cycleway")),
                Side::Right => format!("way/{}{}/{}/right", way.id, seg, obj.prefix.unwrap_or("cycleway")),
            };
            derived.insert("id".into(), Value::String(id.clone()));
            derived.insert("length_m".into(), Value::Number(serde_json::Number::from_f64(seg_len).unwrap()));
            derived.insert("total_length_m".into(), Value::Number(serde_json::Number::from_f64(length_m).unwrap()));
            id
        };

        if segments.len() == 1 {
            let (seg_idx, geom_ewkb, seg_len) = segments.into_iter().next().unwrap();
            let id = finalize(&mut derived, seg_idx, seg_len);
            rows.push(TopicRow {
                osm_id: way.id,
                osm_type: "W",
                id,
                osm,
                derived,
                private,
                meta: meta.clone(),
                geom_ewkb,
                minzoom,
            });
        } else {
            for (seg_idx, geom_ewkb, seg_len) in segments {
                let mut d = derived.clone();
                let id = finalize(&mut d, seg_idx, seg_len);
                rows.push(TopicRow {
                    osm_id: way.id,
                    osm_type: "W",
                    id,
                    osm: osm.clone(),
                    derived: d,
                    private: private.clone(),
                    meta: meta.clone(),
                    geom_ewkb,
                    minzoom,
                });
            }
        }
    }

    rows
}

/// Segments a way's geometry for output: `(segment index, EWKB, length_m)` per row.
///
/// When `split` is set and the way has interior intersection cut-points (more than the two
/// start/end points), each consecutive `cut_points` pair becomes a sub-linestring
/// (`geom.0[start..=end]`, inclusive so the shared node joins both neighbours), with its length
/// measured on the WGS84 `way.coords` slice. Otherwise a single whole-way segment (index `None`).
fn segment_geoms(
    way: &OsmWay,
    geom: &geo::LineString<f64>,
    length_m: f64,
    split: bool,
) -> Vec<(Option<usize>, Vec<u8>, f64)> {
    if split && way.cut_points.len() > 2 {
        way.cut_points
            .windows(2)
            .enumerate()
            .map(|(i, w)| {
                let (s, e) = (w[0].0 as usize, w[1].0 as usize);
                let seg_line = geo::LineString::new(geom.0[s..=e].to_vec());
                let seg_len = haversine_length_m(&way.coords[s..=e]);
                (Some(i), to_ewkb(&seg_line), seg_len)
            })
            .collect()
    } else {
        vec![(None, to_ewkb(geom), length_m)]
    }
}
