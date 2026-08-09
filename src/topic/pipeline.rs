use std::borrow::Cow;

use serde_json::{Map, Value};

use crate::categorize::categories::{categorize, categorize_linear};
use crate::lang::filter::eval_filter;
use crate::lang::producer::ExtractCtx;
use crate::categorize::transform::{run_transform_steps, TransformStep};
use crate::topic::runner::TopicRunner;
use crate::topic::spec::{Field, IdType};
use crate::osm::types::{ElementKind, RawTags};
use crate::osm::types::WayMeta;
use crate::output::rows::TopicRow;

// ── Field evaluation ────────────────────────────────────────────────────────────

/// Evaluate each `Field`'s producer against `ctx`, inserting non-empty results into `produced`.
/// When a value carries provenance, also emit `<output>_source` / `<output>_confidence` into
/// `annotations` — engine bookkeeping about how `produced`'s values came about, not itself a
/// topic-authored output (see `TopicRow::annotations`). Every field's `output` is unique within
/// `fields` (guaranteed by construction — see `runner::resolve_producers`, built from a JSON map
/// keyed by output), so later fields never race earlier ones for the same key: a const default
/// reaches `produced` only via a field whose own producer is a `Fallback` ending in that const
/// (see `runner::merge_const_fields`), which is why no separate "did the const survive" tracking
/// is needed here.
fn eval_fields(
    fields: &[Field],
    ctx: &ExtractCtx,
    produced: &mut Map<String, Value>,
    extra_annotations: &mut Map<String, Value>,
) {
    for field in fields {
        if let Some(p) = field.source.eval(ctx) {
            produced.insert(field.output.clone(), p.value);
            // Companion annotate → `<output>_<k>` (e.g. surface_source, smoothness_confidence).
            // Written into a fresh `extra_annotations` map, separate from `ctx.annotations` (the
            // row's base map) — so the base never needs cloning just to make room for writes; the
            // caller merges the two only if `extra_annotations` actually ends up non-empty (most
            // topics have no `annotate` on their producers at all, e.g. `roads`).
            for (k, v) in p.annotate {
                extra_annotations.insert(format!("{}_{}", field.output, k), v);
            }
        }
    }
}

// ── Public entry point ────────────────────────────────────────────────────────

/// The base object's annotations, shared rather than rebuilt per element.
///
/// `{"_side": "self"}` only for a kind that actually side-splits (`TopicRunner::stamps_side`), so
/// that such a topic's rows uniformly carry the key and a consumer can read it without a coalesce.
/// For every other kind it's empty: stamping a constant `"self"` on every row of a topic that never
/// splits stored 22 bytes/row of nothing (9.5 GB on a 434M-row import) and cloned a one-entry map
/// per emitted row to do it.
///
/// Either way matching is unaffected — readers treat an absent `_side` as "self"
/// (`lang::producer::side_of`).
fn base_annotations(stamps_side: bool) -> &'static Map<String, Value> {
    static SELF: std::sync::OnceLock<Map<String, Value>> = std::sync::OnceLock::new();
    static EMPTY: std::sync::OnceLock<Map<String, Value>> = std::sync::OnceLock::new();
    if stamps_side {
        SELF.get_or_init(|| {
            let mut m = Map::new();
            m.insert("_side".to_owned(), Value::String("self".to_owned()));
            m
        })
    } else {
        EMPTY.get_or_init(Map::new)
    }
}

/// A stack-allocated `"{prefix}/{osm_id}"`. Longest possible content is `relation/` plus a 20-char
/// i64, so 32 bytes always suffices — sized to keep the default id off the heap, since it's built
/// for every element scanned but consumed only by the ones that emit a row.
struct IdBuf {
    buf: [u8; 32],
    len: usize,
}

impl IdBuf {
    fn new(kind: ElementKind, osm_id: i64, id_type: IdType) -> Self {
        use std::fmt::Write;
        let mut b = IdBuf { buf: [0; 32], len: 0 };
        // Infallible for the shapes above; a truncating write would still yield a valid `&str`.
        let _ = match id_type {
            IdType::TypeId => write!(b, "{}/{}", kind.id_prefix(), osm_id),
            // `IdType::None` still builds a base id here: a side-split `Clone` derives its own id
            // from it, and `run_transform_steps` needs that even when the column is dropped. It
            // simply never reaches a row (see `emit`), and `None` is rejected for side-splitting
            // topics anyway.
            IdType::Id | IdType::None => write!(b, "{osm_id}"),
        };
        b
    }

    fn as_str(&self) -> &str {
        // Only ever written through `write_str` below, which appends whole `&str`s, so the prefix
        // is valid UTF-8 by construction.
        std::str::from_utf8(&self.buf[..self.len]).unwrap_or("")
    }
}

impl std::fmt::Write for IdBuf {
    fn write_str(&mut self, s: &str) -> std::fmt::Result {
        let end = (self.len + s.len()).min(self.buf.len());
        let n = end - self.len;
        self.buf[self.len..end].copy_from_slice(&s.as_bytes()[..n]);
        self.len = end;
        Ok(())
    }
}

pub fn build_topic_rows<'a>(
    runner: &TopicRunner,
    kind: ElementKind,
    osm_id: i64,
    raw_tags: &'a RawTags<'a>,
    meta: &WayMeta,
) -> Vec<TopicRow> {
    // The category set for this element kind. Absent → either this kind is flagged `accept_all`
    // (every element passes straight through, no category match — see `TopicSpec::accept_all`) or
    // the topic simply has no categories for this kind at all.
    let categories = match runner.categories.get(&kind) {
        Some(c) => Some(c),
        None if runner.accept_all.contains(&kind) => None,
        None => return Vec::new(),
    };

    // `exclude_condition` is checked first, against raw tags — see `topic::spec::KindTransformsSpec`'s
    // own doc on why it no longer needs anything from the transform pipeline pre-computed for it
    // (a construction-highway rewrite dependency used to force a `before_exclude`/`after_exclude`
    // split; that dependency now lives directly in `exclude_condition` itself, e.g.
    // `configs/tilda/macros.json`'s `is_allowed_highway`). Checking first — against the still-borrowed
    // `raw_tags` — also means an excluded element never pays for a tags clone at all.
    if let Some(cond) = &runner.exclude_condition {
        if eval_filter(cond, raw_tags) {
            return Vec::new();
        }
    }

    // Both of these are built for every element that reaches here but read only by an element that
    // actually matches a category — the overwhelming minority on a nodes pass. `default_id` goes on
    // the stack (see `IdBuf`), and the base annotations start as a borrow of one shared static
    // (see `self_annotations`), cloned into an owned map only at emit time or when a transform
    // pipeline needs to mutate it.
    let default_id_buf = IdBuf::new(kind, osm_id, runner.spec.id_type);
    let default_id = default_id_buf.as_str();
    let mut clones = Vec::new();

    // This kind's own transform pipeline (in-place `InputTransform`s + `Clone`s from
    // `split_sides`), if `transforms.json` defines one — a kind with none simply has an empty
    // slice, a no-op. Only clone `raw_tags` into an owned, mutable copy when there's actually a
    // pipeline to run against it — a kind with no transforms just borrows `raw_tags` as-is.
    static EMPTY_PIPELINE: Vec<TransformStep> = Vec::new();
    let pipeline = runner.pipelines.get(&kind).unwrap_or(&EMPTY_PIPELINE);

    let mut tags: Cow<'a, RawTags<'a>> = if pipeline.is_empty() {
        Cow::Borrowed(raw_tags)
    } else {
        Cow::Owned(raw_tags.clone())
    };

    // A pipeline stamps `_prefix`/`_infix` into the base annotations, so that case (and only that
    // case) needs an owned map up front; everything else keeps borrowing the shared static.
    let mut base_annotations: Cow<'static, Map<String, Value>> =
        Cow::Borrowed(base_annotations(runner.stamps_side(kind)));
    if !pipeline.is_empty() {
        if !run_transform_steps(
            tags.to_mut(),
            base_annotations.to_mut(),
            pipeline,
            default_id,
            &mut clones,
        ) {
            return Vec::new();
        }
    }

    let mut rows = Vec::new();
    // Takes ownership of `base_annotations` (`_side`, plus `_prefix`/`_infix` for a side object —
    // stamped above / by each `Clone`) rather than borrowing it, so it can be moved straight into
    // the row unchanged when nothing needs merging into it — the common case, since most topics'
    // producers carry no `annotate` provenance at all (e.g. `roads`). `ectx` only ever borrows
    // `base_annotations` for the category-match + `eval_fields` calls below, so by the time this
    // closure reaches the merge step that borrow has already ended and moving is fine.
    let mut emit = |obj_tags: &RawTags, parent_tags: Option<&RawTags>, id: &str, base_annotations: Cow<'_, Map<String, Value>>| {
        let ectx = ExtractCtx { obj_tags, parent_tags, id, annotations: &base_annotations };

        // `accept_all` kinds (`categories` is `None`) skip category matching entirely — every
        // element is kept, with no `category` value and `default_producers` as its producers.
        let category_id = match categories {
            Some(cats) => {
                let category = if runner.linear_classify {
                    categorize_linear(&ectx, cats)
                } else {
                    categorize(&ectx, cats)
                };
                let Some(category) = category else {
                    return;
                };
                Some(category.id.clone())
            }
            None => None,
        };

        // This category's effective producers (topic `producers` ⊕ category `producers`, see
        // `TopicSpec::producers`), plus this category's effective `defaults` (folded in as each
        // default's lowest-priority `Fallback` branch — see `runner::merge_default_fields`), share
        // one column and one eval pass.
        let mut produced = Map::new();
        let mut extra_annotations = Map::new();
        let producers = category_id
            .as_ref()
            .and_then(|id| runner.category_producers.get(id))
            .unwrap_or(&runner.default_producers);
        eval_fields(producers, &ectx, &mut produced, &mut extra_annotations);
        if runner.pass_through_remaining_tags {
            // `"producers": true` or a `null` `passthrough_tags` entry (see
            // `TopicRunner::pass_through_remaining_tags`'s own doc) — every raw tag `eval_fields`
            // above didn't already produce a value for, verbatim. `producers: true` means `producers`
            // was empty to begin with, so this ends up filling every key in that case.
            //
            // `produced` empty (the common case for a pure-passthrough topic like `live_raw`, whose
            // category has no explicit fields at all) means there's nothing a wildcard key could
            // collide with — every raw tag is new by construction, so a plain `insert` is safe and
            // skips the extra vacant/occupied branch `.entry(...).or_insert_with(...)` pays per tag
            // just to protect a precedence rule that can't apply here.
            if produced.is_empty() {
                for (k, v) in ectx.obj_tags.iter() {
                    produced.insert(k.to_string(), Value::String(v.to_string()));
                }
            } else {
                // An explicit output (named passthrough or a real derived field) always wins over
                // the wildcard's raw value for the same key, never the other way around.
                for (k, v) in ectx.obj_tags.iter() {
                    produced.entry(k.to_string()).or_insert_with(|| Value::String(v.to_string()));
                }
            }
        }
        // Skipped entirely for a topic that dropped the column (`IdType::None`) — that also
        // avoids building the `String` per row, not just storing it.
        let id = runner.spec.id_type.emits_column().then(|| ectx.id.to_owned());

        // `base_annotations`'s borrow (via `ectx`) ended at the last `eval_fields`/`obj_tags` use
        // above, so it's free to move now: straight through if `eval_fields` added nothing, merged
        // with `extra_annotations` (via the cheaper of the two `append`s) otherwise.
        // Past the category match, so this element is definitely emitting: now is the first point
        // it's worth owning the annotations (a no-op clone away from the shared static) rather than
        // having built a fresh map per element scanned.
        let mut annotations = base_annotations.into_owned();
        if !extra_annotations.is_empty() {
            annotations.append(&mut extra_annotations);
        }

        // JSON-encode here, on whichever rayon classify worker is running this element, instead of
        // handing the raw `Map`s to the writer task — see `TopicRow::produced`'s own doc. `Map`'s
        // keys are always `String` and `Value`'s own `Serialize` impl never fails for the shapes
        // produced here, so `to_string` failing isn't a real case; `unwrap_or_default` just avoids
        // plumbing a `Result` through this closure for it.
        let produced = serde_json::to_string(&produced).unwrap_or_default();

        // An empty map is the base object with no `annotate` provenance — nothing worth a column
        // value, so the row stores NULL rather than `{}` (see `output::rows::jsonb_or_null`).
        let annotations = if annotations.is_empty() {
            String::new()
        } else {
            serde_json::to_string(&annotations).unwrap_or_default()
        };

        // `OsmMeta` is built here, per emitted row, rather than once per element scanned — see
        // `processing::meta_from` for why that distinction dominates the nodes pass. Skipped
        // entirely (column left NULL) for a topic that opted out via `"meta": false`.
        let meta = if runner.spec.meta {
            serde_json::to_string(&crate::processing::meta_from(meta)).unwrap_or_default()
        } else {
            String::new()
        };

        // One tag row per transformed object; geometry (and its per-segment length) lives in the
        // geom table (see `build_edges`), joined on `osm_id` at materialization time. `id`
        // is the self object's own id, or a side object's `"{id}/{prefix}/{side}"`. `category`/
        // `id` are dedicated `TopicRow` columns, not `produced` keys — see `TopicRow::category`.
        rows.push(TopicRow { osm_id, osm_type: kind.osm_type(), id, category: category_id, produced, annotations, meta });
    };

    emit(&*tags, None, default_id, base_annotations);
    for (clone_tags, clone_annotations, id) in clones {
        emit(&clone_tags, Some(&*tags), &id, Cow::Owned(clone_annotations));
    }

    rows
}
