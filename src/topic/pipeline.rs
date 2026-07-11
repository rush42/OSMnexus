use serde_json::{Map, Value};

use crate::tag_engine::categories::categorize;
use crate::tag_engine::filter::eval_filter;
use crate::tag_engine::producer::ExtractCtx;
use crate::tag_engine::transform::side_split::generate_sides;
use crate::topic::runner::TopicRunner;
use crate::topic::spec::Field;
use crate::osm::types::{ElementKind, RawTags};
use crate::output::{rows::TopicRow, types::OsmMeta};

// ── Field evaluation ────────────────────────────────────────────────────────────

/// Evaluate each `Field`'s producer against `ctx`, inserting non-empty results into `map`.
/// When a value carries provenance, also emit `<output>_source` / `<output>_confidence`. Every
/// field's `output` is unique within `fields` (guaranteed by construction — see
/// `runner::resolve_outputs`, built from a JSON map keyed by output), so later fields never race
/// earlier ones for the same key: a const default reaches `map` only via a field whose own
/// producer is a `Fallback` ending in that const (see `runner::merge_const_fields`), which is why
/// no separate "did the const survive" tracking is needed here.
fn eval_fields(fields: &[Field], ctx: &ExtractCtx, map: &mut Map<String, Value>) {
    for field in fields {
        if let Some(p) = field.source.eval(ctx) {
            map.insert(field.output.clone(), p.value);
            // Companion consts → `<output>_<k>` (e.g. surface_source, smoothness_confidence).
            for (k, v) in p.consts {
                map.insert(format!("{}_{}", field.output, k), v);
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
    let topic = &runner.spec;
    // The category set for this element kind. Absent → the topic has no categories for this kind.
    let categories = match runner.categories.get(&kind) {
        Some(c) => c,
        None => return Vec::new(),
    };

    // Pre-categorization tag mutations are way-oriented; nodes/relations never carry them. Split
    // around `exclude_condition` at `exclude_check_at`: tag rewrites (e.g. lifecycle's
    // construction→real-highway swap) run first, so `exclude_condition`'s `is_allowed_highway`
    // check sees the un-construction'd highway — but sidepath-unnest runs *after*
    // `exclude_condition`, since promoting a `cycleway:access=no`-style tag onto bare `access`
    // must not retroactively trigger `exclude_condition`'s own direct `access`/`bicycle`/`foot`
    // checks (which only ever saw the pre-unnest tags in the original pipeline).
    if kind == ElementKind::Way {
        for step in &runner.input_transforms[..runner.exclude_check_at] {
            step.apply(&mut tags, None, "self", None, None);
        }
    }

    if let Some(cond) = &topic.exclude_condition {
        if eval_filter(cond, &tags) {
            return Vec::new();
        }
    }

    if kind == ElementKind::Way {
        for step in &runner.input_transforms[runner.exclude_check_at..] {
            step.apply(&mut tags, None, "self", None, None);
        }
    }

    // Side-split (center-line) transforms are way-oriented; nodes/relations are never side-split.
    let no_transforms = Vec::new();
    let transformations = if kind == ElementKind::Way { &runner.transformations } else { &no_transforms };
    let default_id = format!("{}/{}", kind.id_prefix(), osm_id);
    // Moves `tags` into the self object rather than cloning it (the common no-side-split case).
    // Post-split `directed_keys`/`self_directed_keys` steps are applied inside this call too — no
    // side-specific logic lives here, only the per-`ExtractCtx` callback below.
    let mut rows = Vec::new();

    generate_sides(tags, transformations, &default_id, |ectx| {
        let Some(category) = categorize(&ectx, categories) else {
            return;
        };

        // This category's effective outputs (topic `outputs` ⊕ category `outputs`, see
        // `TopicSpec::outputs`), plus this category's effective consts (folded in as each const
        // output's lowest-priority `Fallback` branch — see `runner::merge_const_fields`), share
        // one column and one eval pass.
        let outputs = runner
            .category_outputs
            .get(&category.id)
            .unwrap_or(&runner.default_outputs);
        let mut derived = Map::new();
        eval_fields(outputs, &ectx, &mut derived);

        // Private consts (`_`-prefixed `consts` keys): nothing else ever targets these, so they're
        // seeded into `private` unconditionally rather than folded into a producer chain.
        let mut private = Map::new();
        if let Some(privates) = runner.category_private_consts.get(&category.id) {
            for (k, v) in privates {
                private.insert(k.clone(), v.clone());
            }
        }

        derived.insert("category".into(), Value::String(category.id.clone()));

        // Side-split context, written generically via `SplitContext::iter` (`_side`, plus
        // `_prefix`/`_infix` for a side object); `_parent_highway` is gone (redundant with the
        // parent's own `highway` tag, already reachable through `ectx.parent_tags`).
        for (k, v) in ectx.split.iter() {
            private.insert(format!("_{k}"), Value::String(v.to_owned()));
        }

        // One tag row per transformed object; geometry (and its per-segment length) lives in the
        // geom table (see `build_geom_rows`), joined on `osm_id` at materialization time. `ectx.id`
        // is the self object's own id, or a side object's `"{id}/{prefix}/{side}"` — computed once
        // by `generate_sides` rather than re-derived here.
        derived.insert("id".into(), Value::String(ectx.id.to_owned()));

        rows.push(TopicRow {
            osm_id,
            osm_type: kind.osm_type(),
            id: ectx.id.to_owned(),
            derived,
            private,
            meta: meta.clone(),
        });
    });

    rows
}

