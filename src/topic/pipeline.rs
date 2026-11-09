use std::borrow::Cow;

use serde_json::{Map, Value};

use crate::categorize::categories::{categorize, categorize_linear};
use crate::lang::filter::eval_filter;
use crate::lang::producer::{Elem, ExtractCtx};
use crate::categorize::transform::{run_transform_steps, TransformStep};
use crate::topic::Topic;
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

/// The base object's annotations before its transforms run: none. Anything a topic wants on its
/// base objects (e.g. `_side: "self"` next to its side objects' `left`/`right`) it writes itself
/// with an `annotate` step, so an empty shared map is all every element starts from — cloned into an
/// owned one only when a transform pipeline needs to write to it.
fn base_annotations() -> &'static Map<String, Value> {
    static EMPTY: std::sync::OnceLock<Map<String, Value>> = std::sync::OnceLock::new();
    EMPTY.get_or_init(Map::new)
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

/// Run the finished pipeline against an empty tag map, once per handled kind, and collect the
/// kinds that produced nothing — the input to `Topic::set_skip_untagged` (see `Topic`'s
/// `skip_untagged` doc for why this is a probe of the real matcher rather than an analysis of
/// filter shapes). Lives on the run side because it executes the pipeline.
pub fn probe_skip_untagged(topic: &Topic) -> std::collections::HashSet<ElementKind> {
    let empty_tags = RawTags::default();
    let no_meta = WayMeta { timestamp: None, user: None, changeset: None };
    [ElementKind::Node, ElementKind::Way, ElementKind::Relation]
        .into_iter()
        .filter(|&kind| topic.has_kind(kind) && build_topic_rows(topic, kind, 0, &empty_tags, &no_meta, &[]).is_empty())
        .collect()
}

/// Run a topic's pipeline for one element of `kind`: a no-op for a kind the topic doesn't handle,
/// otherwise `build_topic_rows`, which applies `kind`'s own transform pipeline (if any),
/// `exclude_condition`, side-split, categorize/extract into tag rows against the kind's category
/// set. `raw_tags` are the element's untouched tags — `build_topic_rows` only clones them if it
/// actually needs to mutate a copy (an excluded element, or one with no transform pipeline, never
/// pays for it).
pub fn process<'a>(
    topic: &Topic,
    kind: ElementKind,
    osm_id: i64,
    raw_tags: &'a RawTags<'a>,
    meta: &WayMeta,
    parents: &[crate::topic::inherit::Parent],
) -> Vec<TopicRow> {
    if !topic.has_kind(kind) {
        return Vec::new();
    }
    build_topic_rows(topic, kind, osm_id, raw_tags, meta, parents)
}

pub fn build_topic_rows<'a>(
    runner: &Topic,
    kind: ElementKind,
    osm_id: i64,
    raw_tags: &'a RawTags<'a>,
    meta: &WayMeta,
    // Parent relations this element inherits from — every one this topic kept (`topic::inherit`).
    // Empty for every kind but ways, and for every topic that declares no `inherit_to_member`.
    parents: &[crate::topic::inherit::Parent],
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

    // A pipeline may write annotations, so that case (and only that case) needs an owned map up
    // front; everything else keeps borrowing the shared empty map.
    let mut base_annotations: Cow<'static, Map<String, Value>> =
        Cow::Borrowed(base_annotations());
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
    let this_elem = Elem { kind, osm_id, timestamp: meta.timestamp };
    let mut emit = |obj_tags: &RawTags,
                    parent: Option<(&RawTags, Elem)>,
                    role: Option<&str>,
                    id: &str,
                    base_annotations: Cow<'_, Map<String, Value>>| {
        let ectx = ExtractCtx {
            obj_tags,
            parent_tags: parent.map(|(t, _)| t),
            id,
            annotations: &base_annotations,
            elem: Some(this_elem),
            parent_elem: parent.map(|(_, e)| e),
            role,
            layer: Default::default(),
        };

        // `accept_all` kinds (`categories` is `None`) skip category matching entirely — every
        // element is kept, with no `category` value and `default_producers` as its producers.
        // Carry the matched category itself, not its id: the row's `category` value and its
        // producers both hang off it directly, so neither costs a `String` clone or a hash of one.
        let matched = match categories {
            Some(cats) => {
                let category = if runner.linear_classify {
                    categorize_linear(&ectx, cats)
                } else {
                    categorize(&ectx, cats)
                };
                let Some(category) = category else {
                    return;
                };
                Some(category)
            }
            None => None,
        };

        // This category's effective producers (topic `producers` ⊕ category `producers`, see
        // `TopicSpec::producers`), plus this category's effective `defaults` (folded in as each
        // default's lowest-priority `Fallback` branch — see `runner::merge_default_fields`), share
        // one column and one eval pass.
        let mut produced = Map::new();
        let mut extra_annotations = Map::new();
        let producers = matched
            .and_then(|c| runner.category_producers.get(c.idx))
            .unwrap_or(&runner.default_producers);
        eval_fields(producers, &ectx, &mut produced, &mut extra_annotations);
        if runner.pass_through_remaining_tags {
            // `"producers": true` or a `null` `passthrough_tags` entry (see
            // `Topic::pass_through_remaining_tags`'s own doc) — every raw tag `eval_fields`
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
                // A refcount bump on the shared handle built at load time, not a fresh allocation — this
        // runs once per emitted row (~14M on a germany tilda run).
        let category = matched.and_then(|c| c.id_shared.clone());
        rows.push(TopicRow { osm_id, osm_type: kind.osm_type(), id, category, produced, annotations, meta });
    };

    // The parent relation is handed to the base object as its `parent_tags`, so a topic reads
    // inherited values with the ordinary `{"parent": {"key": ..}}` producer syntax and *chooses*
    // what to output from them — rather than having them merged into `produced` behind its back.
    // Fanning out here, before categorization, is what makes that work: each row is categorized and
    // evaluated against its own parent, so a category condition can branch on the parent too.
    match (parents.is_empty(), runner.spec.inherit_to_member.as_ref().map(|s| s.mode)) {
        (false, Some(crate::topic::spec::InheritMode::FanOut)) => {
            for p in parents {
                let id = format!("{default_id}/relation/{}", p.rel_id);
                let parent = Elem { kind: ElementKind::Relation, osm_id: p.rel_id, timestamp: p.record.timestamp };
                emit(&*tags, Some((&p.record.fields, parent)), p.role.as_deref(), &id, base_annotations.clone());
            }
        }
        (false, Some(crate::topic::spec::InheritMode::Merge)) => {
            // Merged fields resolve "later parent wins"; the parent's facts (`@id`, `@timestamp`,
            // this way's `@role`) follow the same rule, so they describe the parent whose values won.
            let merged = crate::topic::inherit::merge_parents(parents);
            let last = parents.last().expect("non-empty parents");
            let parent = Elem { kind: ElementKind::Relation, osm_id: last.rel_id, timestamp: last.record.timestamp };
            emit(&*tags, Some((&merged, parent)), last.role.as_deref(), default_id, base_annotations);
        }
        _ => emit(&*tags, None, None, default_id, base_annotations),
    }
    // Side objects keep the base object as their parent, as they always have — a topic that both
    // side-splits and inherits is not supported, and no config does both.
    for (clone_tags, clone_annotations, id) in clones {
        emit(&clone_tags, Some((&*tags, this_elem)), None, &id, Cow::Owned(clone_annotations));
    }

    rows
}

#[cfg(test)]
mod skip_untagged_tests {
    use crate::osm::types::ElementKind;
    use crate::topic::Topic;

    /// Whichever topic comes first in the default config — these tests mutate a loaded topic to
    /// construct the case they need rather than relying on a particular config's contents.
    fn any_runner() -> Topic {
        let runners = crate::config_dir::Config::default_root().and_then(|c| c.load_all(6)).expect("loading topics");
        runners.into_iter().next().expect("config has at least one topic")
    }

    /// `accept_all` keeps every element regardless of tags, so untagged nodes must never be
    /// skipped — the case where the optimization has to switch itself off or rows would be lost.
    ///
    /// `exclude_condition` is cleared to isolate `accept_all`: a topic whose exclude filter already
    /// rejects untagged elements is *still* legitimately skippable, so leaving a real config's
    /// filter in place would test the wrong thing (and did, on first writing).
    #[test]
    fn accept_all_never_skips_untagged() {
        let mut r = any_runner();
        r.categories.remove(&ElementKind::Node);
        r.exclude_condition = None;
        r.accept_all.insert(ElementKind::Node);
        assert!(
            !crate::topic::pipeline::probe_skip_untagged(&r).contains(&ElementKind::Node),
            "an accept_all node topic matches untagged nodes, so skipping them would drop rows"
        );
    }

    /// The converse of the above, and the case that motivates probing rather than analysing filter
    /// shapes: an `exclude_condition` satisfied by an untagged element makes the kind skippable even
    /// though `accept_all` would otherwise keep everything.
    #[test]
    fn exclude_condition_can_make_accept_all_skippable() {
        let mut r = any_runner();
        r.categories.remove(&ElementKind::Node);
        r.accept_all.insert(ElementKind::Node);
        // `railway` absent → true for an untagged element, so everything untagged is excluded.
        r.exclude_condition = Some(
            serde_json::from_value(serde_json::json!({ "not": { "key": "railway", "exists": true } }))
                .expect("filter parses"),
        );
        assert!(crate::topic::pipeline::probe_skip_untagged(&r).contains(&ElementKind::Node));
    }

    /// A kind the topic doesn't handle at all is trivially skippable, and `skips_untagged` reports
    /// that without the probe having to say anything about it.
    #[test]
    fn unhandled_kind_is_skippable() {
        let mut r = any_runner();
        r.categories.remove(&ElementKind::Node);
        r.accept_all.remove(&ElementKind::Node);
        assert!(r.skips_untagged(ElementKind::Node));
    }

    /// The probe must agree with the real pipeline for every kind the loaded config handles: if it
    /// says a kind is skippable, `build_topic_rows` on empty tags really does yield nothing. This is
    /// what makes the optimization safe against a filter that's satisfied by a tag's *absence*
    /// (a negated existence check), which no filter-shape heuristic would catch reliably.
    #[test]
    fn probe_matches_real_pipeline_on_empty_tags() {
        use crate::osm::types::{RawTags, WayMeta};
        for r in crate::config_dir::Config::default_root().and_then(|c| c.load_all(6)).expect("loading topics") {
            for kind in [ElementKind::Node, ElementKind::Way, ElementKind::Relation] {
                if !r.has_kind(kind) {
                    continue;
                }
                let rows = crate::topic::pipeline::build_topic_rows(
                    &r,
                    kind,
                    0,
                    &RawTags::default(),
                    &WayMeta { timestamp: None, user: None, changeset: None },
                    &[],
                );
                assert_eq!(
                    r.skips_untagged(kind),
                    rows.is_empty(),
                    "[{}] {kind:?}: skips_untagged disagrees with the pipeline on empty tags",
                    r.table()
                );
            }
        }
    }
}

/// `inherit_to_member` end to end over the real pipeline: a relation's row is captured into an
/// `InheritStore`, and its member way reads the exported value with the ordinary `{"parent": ..}`
/// producer syntax. Built from a scratch config on disk so the topic goes through the same loader
/// (`config_dir::Config`) every real run uses.
#[cfg(test)]
mod inherit_tests {
    use crate::osm::types::{ElementKind, RawTags, RelData, WayMeta};
    use crate::output::rows::TopicRow;
    use crate::topic::inherit::InheritStore;
    use crate::topic::Topic;

    fn meta() -> WayMeta {
        WayMeta { timestamp: None, user: None, changeset: None }
    }

    fn tags(pairs: &[(&'static str, &'static str)]) -> RawTags<'static> {
        pairs.iter().map(|&(k, v)| (k.into(), v.into())).collect()
    }

    fn produced(row: &TopicRow) -> serde_json::Map<String, serde_json::Value> {
        serde_json::from_str(&row.produced).expect("produced is a JSON object")
    }

    /// One topic over relations and ways. The relation passes its `ref` through and exports it; the
    /// way reads it back as `route_ref` from its parent, and keeps its own `ref` (distinct value) to
    /// prove the two reads don't get crossed.
    fn routes_topic(mode: &str) -> (tempfile::TempDir, Vec<Topic>) {
        let dir = tempfile::tempdir().unwrap();
        let topic_dir = dir.path().join("routes");
        std::fs::create_dir_all(&topic_dir).unwrap();
        let topic = serde_json::json!({
            "table": "routes",
            "accept_all": { "way": true, "relation": true },
            "passthrough_tags": ["ref"],
            // A topic-level producer is a *string* reference into `producers.json`. `true` would not
            // be: it means "pass the tag of this name through", i.e. read a `route_ref` tag.
            "producers": {
                "route_ref": "route_ref", "route_id": "route_id", "route_edited": "route_edited",
                "role": "role", "own_id": "own_id", "own_type": "own_type", "own_edited": "own_edited"
            },
            "inherit_to_member": { "fields": ["ref"], "mode": mode },
            "meta": false
        });
        std::fs::write(topic_dir.join("topic.json"), serde_json::to_vec_pretty(&topic).unwrap()).unwrap();
        let producers = serde_json::json!({
            "route_ref": { "parent": { "key": "ref" } },
            "route_id": { "parent": { "key": "@id" } },
            "route_edited": { "parent": { "key": "@timestamp" } },
            "role": { "key": "@role" },
            "own_id": { "key": "@id" },
            "own_type": { "key": "@type" },
            "own_edited": { "key": "@timestamp" }
        });
        // The topic's own `producers.json` — the config-root one is only the `{"shared": ..}` table.
        std::fs::write(topic_dir.join("producers.json"), serde_json::to_vec_pretty(&producers).unwrap()).unwrap();
        let topics = crate::config_dir::Config::read(dir.path(), false).and_then(|c| c.load_all(6)).expect("loading scratch topic");
        (dir, topics)
    }

    /// A relation `id` with tags `rel_tags`, listing way 100 (role `role`) and way 200, edited at
    /// `id * 1000` seconds.
    fn rel<'a>(id: i64, rel_tags: &RawTags<'a>, role: &'a str) -> RelData<'a> {
        use crate::osm::types::MemberRole;
        RelData {
            id,
            tags: rel_tags.clone(),
            member_ways: vec![(200, MemberRole::Unknown), (100, MemberRole::Unknown)],
            member_role_names: vec!["backward", role],
            meta: WayMeta { timestamp: Some(id * 1000), user: None, changeset: None },
        }
    }

    /// Classify relations `rels` (`(osm_id, ref, way 100's role)`) and capture them, then classify
    /// way 100 (edited at 0 s).
    fn member_way_rows(topics: &[Topic], rels: &[(i64, &'static str, &'static str)]) -> Vec<TopicRow> {
        let store = InheritStore::new(topics);
        assert!(store.any());
        for &(id, r, role) in rels {
            let rel_tags = tags(&[("type", "route"), ("ref", r)]);
            let rows = super::build_topic_rows(&topics[0], ElementKind::Relation, id, &rel_tags, &meta(), &[]);
            assert_eq!(rows.len(), 1, "the relation itself is kept");
            store.capture(topics, 0, &rel(id, &rel_tags, role), &rows[0].produced);
        }
        let ids: Vec<i64> = rels.iter().map(|&(id, _, _)| id).collect();
        let parents = store.parents_for(0, 100, &ids);
        assert_eq!(parents.len(), rels.len(), "every kept relation is a parent");
        let way_meta = WayMeta { timestamp: Some(0), user: None, changeset: None };
        super::build_topic_rows(&topics[0], ElementKind::Way, 100, &tags(&[("highway", "service"), ("ref", "W")]), &way_meta, &parents)
    }

    #[test]
    fn member_way_reads_parent_value_fan_out() {
        let (_dir, topics) = routes_topic("fan_out");
        let rows = member_way_rows(&topics, &[(7, "S1", "forward"), (9, "S2", "")]);
        assert_eq!(rows.len(), 2, "one row per parent");
        for (row, (rel, r, role)) in rows.iter().zip([(7, "S1", "forward"), (9, "S2", "")]) {
            let p = produced(row);
            assert_eq!(p.get("route_ref").and_then(|v| v.as_str()), Some(r), "{:?}", row.produced);
            assert_eq!(p.get("ref").and_then(|v| v.as_str()), Some("W"), "own tag stays the way's own");
            assert!(row.id.as_deref().unwrap().ends_with(&format!("/relation/{rel}")), "{:?}", row.id);
            // The parent's identity, as a value rather than buried in the id string.
            assert_eq!(p.get("route_id").and_then(|v| v.as_str()), Some(rel.to_string().as_str()));
            assert_eq!(p.get("own_id").and_then(|v| v.as_str()), Some("100"));
            assert_eq!(p.get("own_type").and_then(|v| v.as_str()), Some("way"));
            // Its role in *this* parent (not way 200's), and both elements' own edit times.
            assert_eq!(p.get("role").and_then(|v| v.as_str()), Some(role));
            assert_eq!(p.get("own_edited").and_then(|v| v.as_str()), Some("1970-01-01T00:00:00Z"));
            let edited = crate::lang::extract::iso8601_utc(rel * 1000);
            assert_eq!(p.get("route_edited").and_then(|v| v.as_str()), Some(edited.as_str()));
        }
    }

    /// A relation that exports none of the declared fields is still a parent: its id is
    /// information on its own.
    #[test]
    fn parent_without_exported_fields_still_fans_out_with_its_id() {
        let (_dir, topics) = routes_topic("fan_out");
        let store = InheritStore::new(&topics);
        let rel_tags = tags(&[("type", "route")]);
        let rows = super::build_topic_rows(&topics[0], ElementKind::Relation, 7, &rel_tags, &meta(), &[]);
        store.capture(&topics, 0, &rel(7, &rel_tags, "forward"), &rows[0].produced);
        let parents = store.parents_for(0, 100, &[7]);
        let rows = super::build_topic_rows(&topics[0], ElementKind::Way, 100, &tags(&[("highway", "service")]), &meta(), &parents);
        assert_eq!(rows.len(), 1);
        let p = produced(&rows[0]);
        assert_eq!(p.get("route_id").and_then(|v| v.as_str()), Some("7"));
        assert!(p.get("route_ref").is_none());
    }

    #[test]
    fn member_way_reads_parent_value_merge() {
        let (_dir, topics) = routes_topic("merge");
        let rows = member_way_rows(&topics, &[(7, "S1", "forward"), (9, "S2", "platform")]);
        assert_eq!(rows.len(), 1, "one row, parents merged");
        // Later parent wins on collision (parents are in sorted-id order) — its facts included.
        let p = produced(&rows[0]);
        assert_eq!(p.get("route_ref").and_then(|v| v.as_str()), Some("S2"), "{:?}", rows[0].produced);
        assert_eq!(p.get("route_id").and_then(|v| v.as_str()), Some("9"));
        assert_eq!(p.get("role").and_then(|v| v.as_str()), Some("platform"));
    }

    /// Without a parent, `{"parent": ..}` produces nothing, and the way is emitted once as usual.
    #[test]
    fn non_member_way_has_no_parent_value() {
        let (_dir, topics) = routes_topic("fan_out");
        let rows = super::build_topic_rows(&topics[0], ElementKind::Way, 100, &tags(&[("highway", "service")]), &meta(), &[]);
        assert_eq!(rows.len(), 1);
        let p = produced(&rows[0]);
        assert!(p.get("route_ref").is_none(), "{:?}", rows[0].produced);
        assert!(p.get("route_id").is_none(), "no parent, no parent id");
        assert!(p.get("role").is_none(), "no parent, no role");
        assert!(p.get("own_edited").is_none(), "no metadata, no timestamp");
        assert_eq!(p.get("own_id").and_then(|v| v.as_str()), Some("100"));
    }
}
