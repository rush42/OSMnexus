use std::collections::HashSet;

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
/// When a value carries provenance, also emit `<output>_source` / `<output>_confidence`.
/// Each produced output key is recorded in `written` so the caller can tell which const
/// defaults were overwritten (used to gate bundled-const companion emission). `written` borrows
/// from `fields` rather than cloning `output` again — it's only ever queried with `.contains()`,
/// never handed out, so there's nothing an owned copy buys here.
/// Used for `osm_fields`, sanitizers, and derivers alike.
fn eval_fields<'a>(
    fields: &'a [Field],
    ctx: &ExtractCtx,
    map: &mut Map<String, Value>,
    written: &mut HashSet<&'a str>,
) {
    for field in fields {
        if let Some(p) = field.source.eval(ctx) {
            map.insert(field.output.clone(), p.value);
            written.insert(&field.output);
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

        let mut osm = Map::new();
        let mut osm_written = HashSet::new();
        eval_fields(&topic.osm_fields, &ectx, &mut osm, &mut osm_written);

        // Sanitizer + deriver outputs share one column and one eval pass: `derivers` is
        // desugared-sanitizers-then-derivers (see `TopicRunner::topic_derivers`), from this
        // category's effective set (topic defaults ± overrides).
        let derivers = runner
            .category_derivers
            .get(&category.id)
            .unwrap_or(&runner.topic_derivers);
        let mut derived = Map::new();
        let mut private = Map::new();
        // Lowest-priority layer: seed category const *values* (bundled entries contribute only
        // their `value` here). A `_`-prefixed key routes into `private` instead of `derived` (no
        // sanitizer/deriver ever targets it, so it's seeded there unconditionally, not layered).
        // Any bundled companions are emitted after field evaluation, and only for `derived` keys
        // no sanitizer/deriver overwrote ("the const wins").
        let consts = runner.category_consts.get(&category.id);
        if let Some(consts) = consts {
            for (k, v) in consts {
                let (value, _) = const_entry(v);
                if k.starts_with('_') {
                    private.insert(k.clone(), value.clone());
                } else {
                    derived.insert(k.clone(), value.clone());
                }
            }
        }
        let mut written = HashSet::new();
        eval_fields(derivers, &ectx, &mut derived, &mut written);

        // Emit bundled-const companions for entries still holding their const default (not
        // produced by a sanitizer/deriver): `<key>_<companion>` into `derived`, mirroring the
        // branch-const provenance rule (e.g. oneway that fell through to the implicit default
        // contributes `oneway_confidence`).
        if let Some(consts) = consts {
            for (k, v) in consts {
                if k.starts_with('_') || written.contains(k.as_str()) {
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
            osm,
            derived,
            private,
            meta: meta.clone(),
        });
    });

    rows
}

