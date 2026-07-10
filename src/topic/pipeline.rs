use std::collections::HashSet;

use serde_json::{Map, Value};

use crate::tag_engine::categories::categorize;
use crate::tag_engine::filter::eval_filter;
use crate::tag_engine::producer::ExtractCtx;
use crate::tag_engine::transform::side_split::{get_transformed_objects, iter_with_ctx};
use crate::topic::runner::TopicRunner;
use crate::topic::spec::Field;
use crate::osm::types::{ElementKind, RawTags};
use crate::output::{
    rows::TopicRow,
    types::{OsmMeta, Side},
};

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
        crate::profile::time(&crate::profile::INPUT_TRANSFORMS, || {
            for step in &runner.input_transforms[..runner.exclude_check_at] {
                step.apply(&mut tags, None, "self", None, None);
            }
        });
    }

    if let Some(cond) = &topic.exclude_condition {
        let excluded = crate::profile::time(&crate::profile::EXCLUDE, || {
            eval_filter(cond, &tags)
        });
        if excluded {
            return Vec::new();
        }
    }

    if kind == ElementKind::Way {
        crate::profile::time(&crate::profile::INPUT_TRANSFORMS, || {
            for step in &runner.input_transforms[runner.exclude_check_at..] {
                step.apply(&mut tags, None, "self", None, None);
            }
        });
    }

    // Side-split (center-line) transforms are way-oriented; nodes/relations are never side-split.
    let no_transforms = Vec::new();
    let transformations = if kind == ElementKind::Way { &runner.transformations } else { &no_transforms };
    let default_id = format!("{}/{}", kind.id_prefix(), osm_id);
    // Moves `tags` into the self object rather than cloning it (the common no-side-split case).
    let mut transformed = crate::profile::time(&crate::profile::SIDESPLIT, || {
        get_transformed_objects(tags, transformations, &default_id)
    });

    // Per-object post-split steps (currently just `directed_keys`/`self_directed_keys`, ported
    // from `split_sides`'s config into ordinary `InputTransform`s): applied to each side object's
    // own tags, using the self object's tags as `parent_tags` — still pre-categorization (it can
    // influence which category a side object matches), just after cardinality is decided, since it
    // needs each object's resolved `side` to pick `:forward`/`:backward`. `side_split` itself only
    // ever does unnesting; this is what makes `directed_keys` an ordinary data-defined transform
    // instead of bespoke logic living inside the split.
    if let [self_obj, side_objs @ ..] = transformed.as_mut_slice() {
        for obj in side_objs {
            let side_str = match obj.side {
                Side::Left => "left",
                Side::Right => "right",
                Side::Self_ => unreachable!("side objects are never Self_"),
            };
            let Some(transformation) = transformations.iter().find(|t| Some(t.prefix) == obj.prefix)
            else { continue };
            for step in transformation.directed_steps {
                step.apply(&mut obj.tags, Some(&self_obj.tags), side_str, obj.prefix, obj.infix);
            }
        }
    }
    let mut rows = Vec::new();

    // `iter_with_ctx` pairs each transformed object with its `ExtractCtx` (side objects' `parent_tags`
    // resolved against the self object) — the only place per-object side/prefix/infix/parent
    // addressing gets assembled; every earlier tag-only transform never touches it. One `ExtractCtx`
    // serves both categorization and field evaluation — same "object state", just consumed by two
    // different evaluators (`Filter`/`Producer`).
    for ectx in iter_with_ctx(&transformed) {
        let category = match crate::profile::time(&crate::profile::CATEGORIZE, || categorize(&ectx, categories)) {
            Some(c) => c,
            None => continue,
        };
        // Times the rest of this iteration (field eval + const seeding + row build).
        let _extract = crate::profile::scope(&crate::profile::EXTRACT);

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
        eval_fields(derivers, &ectx, &mut derived, &mut written);

        // Emit bundled-const companions for entries still holding their const default (not
        // produced by a sanitizer/deriver): `<key>_<companion>` into `derived`, mirroring the
        // branch-const provenance rule (e.g. oneway that fell through to the implicit default
        // contributes `oneway_confidence`).
        if let Some(consts) = consts {
            for (k, v) in consts {
                if written.contains(k.as_str()) {
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

        // Side-split context, merged flat into `private` as before; `_parent_highway` is gone
        // (redundant with the parent's own `highway` tag, already reachable through
        // `ectx.parent_tags` for anything that needs it).
        private.insert("_side".into(), Value::String(ectx.split.obj_side.to_owned()));
        if let Some(p) = ectx.split.prefix {
            private.insert("_prefix".into(), Value::String(p.to_owned()));
        }
        if let Some(i) = ectx.split.infix {
            private.insert("_infix".into(), Value::String(i.to_owned()));
        }

        // One tag row per transformed object; geometry (and its per-segment length) lives in the
        // geom table (see `build_geom_rows`), joined on `osm_id` at materialization time. `ectx.id`
        // is the self object's own id, or a side object's `"{id}/{prefix}/{side}"` — computed once
        // by `get_transformed_objects` rather than re-derived here.
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
    }

    rows
}

