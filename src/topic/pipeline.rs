use serde_json::{Map, Value};

use crate::tag_engine::categories::categorize;
use crate::tag_engine::filter::eval_filter;
use crate::tag_engine::producer::ExtractCtx;
use crate::tag_engine::transform::run_transform_steps;
use crate::topic::runner::TopicRunner;
use crate::topic::spec::Field;
use crate::osm::types::{ElementKind, RawTags};
use crate::output::{rows::TopicRow, types::OsmMeta};

// ── Field evaluation ────────────────────────────────────────────────────────────

/// Evaluate each `Field`'s producer against `ctx`, inserting non-empty results into `produced`.
/// When a value carries provenance, also emit `<output>_source` / `<output>_confidence` into
/// `annotations` — engine bookkeeping about how `produced`'s values came about, not itself a
/// topic-authored output (see `TopicRow::annotations`). Every field's `output` is unique within
/// `fields` (guaranteed by construction — see `runner::resolve_outputs`, built from a JSON map
/// keyed by output), so later fields never race earlier ones for the same key: a const default
/// reaches `produced` only via a field whose own producer is a `Fallback` ending in that const
/// (see `runner::merge_const_fields`), which is why no separate "did the const survive" tracking
/// is needed here.
fn eval_fields(fields: &[Field], ctx: &ExtractCtx, produced: &mut Map<String, Value>, annotations: &mut Map<String, Value>) {
    for field in fields {
        if let Some(p) = field.source.eval(ctx) {
            produced.insert(field.output.clone(), p.value);
            // Companion annotate → `<output>_<k>` (e.g. surface_source, smoothness_confidence).
            for (k, v) in p.annotate {
                annotations.insert(format!("{}_{}", field.output, k), v);
            }
        }
    }
}

// ── Public entry point ────────────────────────────────────────────────────────

pub fn build_topic_rows(
    runner: &TopicRunner,
    kind: ElementKind,
    osm_id: i64,
    mut tags: RawTags,
    meta: &OsmMeta,
) -> Vec<TopicRow> {
    // The category set for this element kind. Absent → the topic has no categories for this kind.
    let categories = match runner.categories.get(&kind) {
        Some(c) => c,
        None => return Vec::new(),
    };

    let default_id = format!("{}/{}", kind.id_prefix(), osm_id);
    let mut annotations = Map::new();
    annotations.insert("_side".to_owned(), Value::String("self".to_owned()));
    let mut clones = Vec::new();

    // The full transform pipeline (in-place `InputTransform`s + `Clone`s from `split_sides`) is
    // way-oriented; nodes/relations never carry them. Split around `exclude_condition` at
    // `exclude_check_at`: tag rewrites (e.g. lifecycle's construction→real-highway swap) run
    // first, so `exclude_condition`'s `is_allowed_highway` check sees the un-construction'd
    // highway — but sidepath-unnest (and every `Clone`, i.e. side-splitting) runs *after*
    // `exclude_condition`, since promoting a `cycleway:access=no`-style tag onto bare `access`
    // must not retroactively trigger `exclude_condition`'s own direct `access`/`bicycle`/`foot`
    // checks (which only ever saw the pre-unnest tags in the original pipeline).
    if kind == ElementKind::Way
        && !run_transform_steps(&mut tags, &mut annotations, &runner.pipeline[..runner.exclude_check_at], &default_id, &mut clones)
    {
        return Vec::new();
    }

    if let Some(cond) = &runner.exclude_condition {
        if eval_filter(cond, &tags) {
            return Vec::new();
        }
    }

    if kind == ElementKind::Way
        && !run_transform_steps(&mut tags, &mut annotations, &runner.pipeline[runner.exclude_check_at..], &default_id, &mut clones)
    {
        return Vec::new();
    }

    let mut rows = Vec::new();
    let mut emit = |ectx: ExtractCtx| {
        let Some(category) = categorize(&ectx, categories) else {
            return;
        };

        // This category's effective outputs (topic `outputs` ⊕ category `outputs`, see
        // `TopicSpec::outputs`), plus this category's effective `defaults` (folded in as each
        // default's lowest-priority `Fallback` branch — see `runner::merge_default_fields`), share
        // one column and one eval pass.
        let outputs = runner
            .category_outputs
            .get(&category.id)
            .unwrap_or(&runner.default_outputs);
        // `ectx.annotations` already carries `_side`, plus `_prefix`/`_infix` for a side object
        // (stamped above / by each `Clone`); clone it as this row's base annotations map, then
        // let `eval_fields` add each output's own `annotate` provenance on top. `_parent_highway`
        // is gone (redundant with the parent's own `highway` tag, already reachable through
        // `ectx.parent_tags`).
        let mut produced = Map::new();
        let mut annotations = ectx.annotations.clone();
        eval_fields(outputs, &ectx, &mut produced, &mut annotations);

        // One tag row per transformed object; geometry (and its per-segment length) lives in the
        // geom table (see `build_geom_rows`), joined on `osm_id` at materialization time. `ectx.id`
        // is the self object's own id, or a side object's `"{id}/{prefix}/{side}"`. `category`/
        // `id` are dedicated `TopicRow` columns, not `produced` keys — see `TopicRow::category`.
        rows.push(TopicRow {
            osm_id,
            osm_type: kind.osm_type(),
            id: ectx.id.to_owned(),
            category: category.id.clone(),
            produced,
            annotations,
            meta: meta.clone(),
        });
    };

    emit(ExtractCtx { obj_tags: &tags, parent_tags: None, id: &default_id, annotations: &annotations });
    for (clone_tags, clone_annotations, id) in &clones {
        emit(ExtractCtx { obj_tags: clone_tags, parent_tags: Some(&tags), id, annotations: clone_annotations });
    }

    rows
}
