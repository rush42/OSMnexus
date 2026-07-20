use serde_json::{Map, Value};

use crate::categorize::categories::categorize;
use crate::lang::filter::eval_filter;
use crate::lang::producer::ExtractCtx;
use crate::categorize::transform::{run_transform_steps, TransformStep};
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
fn eval_fields(
    fields: &[Field],
    ctx: &ExtractCtx,
    produced: &mut Map<String, Value>,
    annotations: &mut Map<String, Value>,
    field_stages: &crate::profiling::FieldStages,
) {
    let _t = crate::profiling::time(&crate::profiling::TAG_ENGINE);
    for field in fields {
        let _tf = field_stages.time(&field.output);
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

    // `exclude_condition` is checked first, against raw tags — see `topic::spec::KindTransformsSpec`'s
    // own doc on why it no longer needs anything from the transform pipeline pre-computed for it
    // (a construction-highway rewrite dependency used to force a `before_exclude`/`after_exclude`
    // split; that dependency now lives directly in `exclude_condition` itself, e.g.
    // `configs/tilda/macros.json`'s `is_allowed_highway`). Checking first also means an excluded
    // element never pays for the transform pipeline at all.
    if let Some(cond) = &runner.exclude_condition {
        let _t = crate::profiling::time(&crate::profiling::EXCLUDE_CHECK);
        if eval_filter(cond, &tags) {
            return Vec::new();
        }
    }

    // This kind's own transform pipeline (in-place `InputTransform`s + `Clone`s from
    // `split_sides`), if `transforms.json` defines one — a kind with none simply has an empty
    // slice, a no-op.
    static EMPTY_PIPELINE: Vec<TransformStep> = Vec::new();
    let pipeline = runner.pipelines.get(&kind).unwrap_or(&EMPTY_PIPELINE);

    {
        let _t = crate::profiling::time(&crate::profiling::TRANSFORM_STEPS);
        if !run_transform_steps(&mut tags, &mut annotations, pipeline, &default_id, &mut clones) {
            return Vec::new();
        }
    }

    let _t_iter = crate::profiling::time(&crate::profiling::ITERATION);
    let mut rows = Vec::new();
    let mut emit = |ectx: ExtractCtx| {
        let category = {
            let _t = crate::profiling::time(&crate::profiling::CATEGORIZE);
            categorize(&ectx, categories)
        };
        let Some(category) = category else {
            return;
        };

        // This category's effective outputs (topic `outputs` ⊕ category `outputs`, see
        // `TopicSpec::outputs`), plus this category's effective `defaults` (folded in as each
        // default's lowest-priority `Fallback` branch — see `runner::merge_default_fields`), share
        // one column and one eval pass.
        let mut produced = Map::new();
        let mut annotations;
        let outputs = {
            let _t = crate::profiling::time(&crate::profiling::ROW_OVERHEAD);
            // `ectx.annotations` already carries `_side`, plus `_prefix`/`_infix` for a side object
            // (stamped above / by each `Clone`); clone it as this row's base annotations map, then
            // let `eval_fields` add each output's own `annotate` provenance on top. `_parent_highway`
            // is gone (redundant with the parent's own `highway` tag, already reachable through
            // `ectx.parent_tags`).
            annotations = ectx.annotations.clone();
            runner.category_outputs.get(&category.id).unwrap_or(&runner.default_outputs)
        };
        eval_fields(outputs, &ectx, &mut produced, &mut annotations, &runner.field_stages);

        {
            let _t = crate::profiling::time(&crate::profiling::ROW_OVERHEAD);
            // One tag row per transformed object; geometry (and its per-segment length) lives in the
            // geom table (see `build_edges`), joined on `osm_id` at materialization time. `ectx.id`
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
        }
    };

    emit(ExtractCtx { obj_tags: &tags, parent_tags: None, id: &default_id, annotations: &annotations });
    for (clone_tags, clone_annotations, id) in &clones {
        emit(ExtractCtx { obj_tags: clone_tags, parent_tags: Some(&tags), id, annotations: clone_annotations });
    }

    rows
}
