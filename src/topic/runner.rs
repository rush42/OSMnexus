use std::collections::HashMap;

use rayon::prelude::*;

use anyhow::Context;
use serde_json::{Map, Value};

use crate::categorize::categories::CategoriesFile;
use crate::lang::filter::Filter;
use crate::lang::producer::Producer;
use crate::categorize::transform::TransformStep;
use crate::topic::load::{
    inline_shared_producers, inline_sanitize_refs, load_shared_macros, load_shared_producers, load_topic_categories,
    load_topic_macros, load_topic_sanitizers, load_topic_transforms, merge, resolve_macros, resolve_refs,
};
use crate::topic::pipeline::build_topic_rows;
use crate::topic::spec::{resolve_producer_entry, Field, GeometryShape, TopicSpec, TransformsSpec};
use crate::osm::types::{ElementKind, RawTags, WayMeta};
use crate::output::rows::TopicRow;

/// A fully loaded topic ready to process ways.
pub struct TopicRunner {
    pub spec: TopicSpec,
    /// Category sets keyed by element kind — one per `topics/<t>/{node,way,relation}/` subfolder
    /// that exists. Each pass (relations → ways → nodes) classifies with its kind's set; a topic
    /// with only a `way/` folder has just the `Way` entry. `categorize` is the same function for all.
    pub categories: HashMap<ElementKind, CategoriesFile>,
    /// Each kind's full transform pipeline, in declared order, always run *after*
    /// `exclude_condition` is checked (see `topic::pipeline::build_topic_rows` and
    /// `topic::spec::KindTransformsSpec`'s own doc on why no phase split is needed) — a mix of
    /// ordinary in-place `InputTransform`s and `Clone`s (from `split_sides`, always synthesized
    /// after every `InputTransform`, one per side). Unlike an output's producer, these can
    /// influence which category an element matches — see
    /// `categorize::transform::run_transform_steps`, which drives the whole thing. Keyed by
    /// `ElementKind`; a kind with no `transforms.json` entry (or no `transforms.json` at all)
    /// simply has no entry here — see `topic::spec::TransformsSpec::into_pipelines`.
    pub pipelines: HashMap<ElementKind, Vec<TransformStep>>,
    /// The topic's `exclude_condition` (`topic.json`), already macro/sanitizer-resolved by the time
    /// `TopicSpec` deserializes it (see `TopicRunner::load`). Held here (taken out of `spec`, not
    /// duplicated) so the runtime pipeline (`topic::pipeline::build_topic_rows`) reads it directly.
    pub exclude_condition: Option<Filter>,
    /// Topic-default fields (`spec.producers`, resolved, with topic-level `defaults` folded in as
    /// each default's lowest-priority `Fallback` branch — see `merge_default_fields`) — the
    /// fallback used if a category id somehow isn't in `category_producers` (shouldn't normally
    /// happen: every category gets its own entry at load time below).
    pub default_producers: Vec<Field>,
    /// Whether this topic's categories were compiled with `tree_max_depth == 0` — when set,
    /// `build_topic_rows` calls `categorize_linear` instead of `categorize`, bypassing the decision
    /// tree entirely (see `categorize::categories`). Derived, not independently configurable: a
    /// depth-0 tree is never built in the first place (`build_order` skips straight to
    /// `DecisionTree::default()`), so this just remembers that choice for the runtime dispatch.
    pub linear_classify: bool,
    /// Per-category effective producers: the topic's `producers` map merged with the category's own
    /// `producers` overrides (category wins, by key — plain JSON-object merge, see `TopicSpec::producers`),
    /// then resolved into `Producer`s, with the category's effective (topic ⊕ category) `defaults`
    /// folded in as a trailing fallback branch on each default's output — so a default
    /// is just the lowest-priority producer for its key, evaluated by the same `eval_fields` pass
    /// as any other output. Present for every category (unlike a plain override map, every
    /// category's effective defaults can still differ from the topic's even with no `producers`
    /// override).
    pub category_producers: HashMap<String, Vec<Field>>,
    /// Set when `topic.json`'s `producers` is the bare `true` shorthand (`ProducersSpec::is_all`) or
    /// `passthrough_tags` carries a bare `null` entry (`TopicSpec::passthrough_tags`'s own doc) —
    /// either way, every raw tag `eval_fields` didn't already produce a value for gets copied into
    /// `produced` verbatim (`topic::pipeline::build_topic_rows`). `producers: true` needs no separate
    /// bypass path: `ProducersSpec::into_fields_map` treats `All` as `{}`, so `default_producers`/
    /// `category_producers` are already empty in that case — running `eval_fields` over zero fields
    /// costs nothing, and this one flag then fills every key either way.
    pub pass_through_remaining_tags: bool,
    /// Kinds flagged in `topic.json`'s `"accept_all"` (see `TopicSpec::accept_all`) — every
    /// (non-excluded) element of this kind is emitted with no category match and no `category`
    /// value, using `default_producers` directly. Disjoint from `categories`'s keys (`load` rejects
    /// a kind declaring both a category directory and `accept_all`).
    pub accept_all: std::collections::HashSet<ElementKind>,
    /// Output names whose topic-level entry is the bare `true` "copy this tag verbatim" shorthand
    /// (an inline `"<tag>": true` or one folded in from `passthrough_tags` — see
    /// `TopicSpec::passthrough_tags`) rather than an authored producer. Once resolved, such an entry
    /// is a `Producer::Extract` indistinguishable in shape from a hand-written single-key extract,
    /// so this has to be captured pre-resolution — used to keep passthrough tags out of
    /// `bin/dag_json`'s "Producer" picker list, where showing them (there's no real producer tree
    /// to plot) is just noise.
    pub passthrough_producers: std::collections::HashSet<String>,
    /// Kinds for which an element with **no tags at all** provably yields no rows, so the caller can
    /// skip it without decoding it at all (see `skips_untagged`).
    ///
    /// Not assumable from "categories exist": a filter can be *tautological on empty tags* — e.g. a
    /// negated existence check (`highway` absent) is true precisely when the element has no tags —
    /// and `accept_all` keeps everything by definition. So this isn't derived by inspecting filter
    /// shapes; it's decided at load time by running the real pipeline
    /// (`build_topic_rows`) against an empty tag map and recording whether it produced anything.
    /// Using the actual matcher means this can't drift from runtime semantics the way a
    /// hand-written static analysis of filter shapes would.
    skip_untagged: std::collections::HashSet<ElementKind>,
    /// Kinds whose transform pipeline side-splits — i.e. contains a `Clone` that annotates
    /// `_side`. Those kinds stamp `_side: "self"` on the base object too, so every row of such a
    /// topic carries the key and a consumer can read it without a coalesce.
    ///
    /// Elsewhere the base object deliberately carries no `_side` at all (see
    /// `topic::pipeline::base_annotations`): for a topic that never splits, the key would be a
    /// constant on every row saying nothing. Matching is unaffected either way — readers treat an
    /// absent `_side` as "self" (`lang::producer::side_of`).
    stamps_side: std::collections::HashSet<ElementKind>,
}

/// Resolve one topic's or category's raw `producers` map (already merged by key, category winning)
/// into a `Vec<Field>` — the actual payoff of keying `producers` by name: no more `apply_overrides`/
/// `resolve_bindings`/`check_unique_producers` Vec-scanning, just one merge then one resolve pass.
/// Duplicate keys can't arise (JSON object keys are inherently unique per map), so no separate
/// uniqueness check is needed either.
fn resolve_producers(
    raw: Map<String, Value>,
    producer_lib: &HashMap<String, Producer>,
    sanitizers: &HashMap<String, Vec<crate::lang::sanitize::Sanitizer>>,
    context: &str,
) -> anyhow::Result<Vec<Field>> {
    raw.into_iter()
        .map(|(output, value)| {
            let source = resolve_producer_entry(&output, value, producer_lib, sanitizers)
                .with_context(|| context.to_owned())?;
            Ok(Field { output, source })
        })
        .collect()
}

/// A `defaults` JSON entry as a `Producer`: a bundled `{ "value": ..., "annotate": {...} }` object
/// carries its companions as the producer's own `annotate` (so `Producer::eval` emits them exactly
/// when this branch produces — no separate "did the default survive" bookkeeping needed
/// elsewhere); any other JSON is a bare literal with no companions. Just a `Const` — it
/// unconditionally "produces"; the default value is only ever *reached* via the two-rule `Fallback`
/// `as_fallback_pair` wraps it in when nothing higher-priority did. Built directly as a `Producer`
/// value (not JSON) after `resolve_producers` has already run, so it deliberately bypasses
/// `Producer::resolve` — fine here since it carries no macro/sanitizer references to resolve.
fn default_value_producer(v: &Value) -> Producer {
    let (value, annotate) = match v {
        Value::Object(obj) if obj.contains_key("value") => {
            (obj["value"].clone(), obj.get("annotate").and_then(Value::as_object).cloned().unwrap_or_default())
        }
        _ => (v.clone(), Map::new()),
    };
    Producer::Const { value, annotate }
}

/// Wrap `primary`/`default_source` as an unconditional (`when: true`) two-rule `Match` — the same
/// shape `resolve()` would produce from `{ "match": [primary, default] }`'s bare-item shorthand,
/// built directly since this runs after `resolve_producers` already resolved both producers (a
/// second `resolve` pass isn't needed).
fn as_fallback_pair(primary: Producer, default_source: Producer) -> Producer {
    let rule = |value: Producer| crate::lang::producer::Rule { when: Filter::Bool(true), value };
    Producer::Match {
        rules: vec![rule(primary), rule(default_source)],
        default: None,
        annotate: Map::new(),
        tree: None,
    }
}

/// Fold `defaults`' keys into `fields` as the lowest-priority producer for their output:
/// appended as a trailing fallback branch onto an existing field targeting that output (so the
/// default only takes effect when the real producer returns `None`), or pushed as a new
/// default-only field when nothing else targets it (e.g. a bare literal like `minzoom`).
fn merge_default_fields(mut fields: Vec<Field>, defaults: &Map<String, Value>) -> Vec<Field> {
    for (k, v) in defaults {
        let default_source = default_value_producer(v);
        match fields.iter_mut().find(|f| &f.output == k) {
            Some(existing) => {
                existing.source = as_fallback_pair(existing.source.clone(), default_source);
            }
            None => fields.push(Field { output: k.clone(), source: default_source }),
        }
    }
    fields
}

impl TopicRunner {
    /// Discover and load every topic under the active config directory. Only directories are
    /// considered (the shared `macros.json`/`sanitizers.json`/`producers.json`/`value_sets.json`/
    /// `units.json` files at the config root are skipped automatically), and any `_`-prefixed
    /// directory is skipped too, as a general hidden-directory convention. Returned in sorted
    /// name order for deterministic output.
    pub fn load_all(tree_max_depth: usize) -> anyhow::Result<Vec<Self>> {
        let topics_dir = crate::paths::config_root();
        let mut names: Vec<String> = std::fs::read_dir(&topics_dir)
            .with_context(|| format!("reading {}", topics_dir.display()))?
            .filter_map(|entry| {
                let entry = entry.ok()?;
                if !entry.file_type().ok()?.is_dir() {
                    return None;
                }
                let name = entry.file_name().to_string_lossy().into_owned();
                (!name.starts_with('_')).then_some(name)
            })
            .collect();
        names.sort();
        names.iter().map(|name| Self::load(name, tree_max_depth)).collect()
    }

    /// Load a topic from its directory `<config_root>/<name>/`. `tree_max_depth == 0` skips
    /// compiling a decision tree entirely (see `build_order`) and switches this topic's category
    /// classification to the linear reference walk (`categorize_linear`) at runtime.
    pub fn load(name: &str, tree_max_depth: usize) -> anyhow::Result<Self> {
        let base = crate::paths::config_root().join(name);
        let config_root = base.parent().expect("topics/<name> has a parent").to_path_buf();

        // Named atomic transforms (`sanitize:` targets), shared+topic-local. A separate
        // registry/namespace from the producer library below — atomic chains and composite
        // producers are different *types* (`Sanitizer`/`Producer`), so there's no risk of a
        // name meaning two things at once. Loaded before macros, since a macro's own condition can
        // carry a `sanitize:` too.
        let sanitizers = load_topic_sanitizers(&base, &config_root)?;

        // Every macro this topic can reference: shared (config root's macros.json) plus the
        // topic's own macros.json, topic-local winning on name conflict — raw JSON, still possibly
        // macro-in-macro. `resolve_macros` expands each entry against itself (recursively,
        // cycle-checked) and inlines any `sanitize:` reference too, producing a fully macro-free
        // JSON per name — what `inline_macro_refs`/`topic::load::resolve_refs` need to substitute
        // `{"macro": "<name>"}` sites everywhere else in this topic's JSON.
        let shared_macros = load_shared_macros(&config_root).with_context(|| "loading shared macros.json")?;
        let topic_macros = load_topic_macros(&base)?;
        let raw_macros = merge(&shared_macros, &topic_macros);
        let resolved_macros = resolve_macros(&raw_macros)
            .with_context(|| format!("resolving macros for topics/{name}"))?;
        // The same table, additionally sanitizer-resolved and deserialized to `Filter` — used only
        // to seed each `CategoriesFile`'s `macros` (whose `build_order` `Skip`-sink `excludes`
        // entries name a macro directly, not through a condition, so they're never touched by
        // JSON-level macro inlining).
        let macros: HashMap<String, Filter> = resolved_macros.iter()
            .map(|(k, v)| Ok((k.clone(), serde_json::from_value(inline_sanitize_refs(v.clone(), &sanitizers)?)?)))
            .collect::<anyhow::Result<_>>()
            .with_context(|| format!("parsing resolved macros for topics/{name}"))?;

        // `topic.json`, fully macro/sanitizer-resolved before it's ever deserialized into
        // `TopicSpec` — so `exclude_condition` lands as a plain `Option<Filter>` and `producers`'/
        // `defaults`' values (still untyped `Value`s at this level) already carry no unresolved
        // reference either, letting `resolve_producers` below skip a further resolve pass.
        let raw_topic: Value = serde_json::from_str(
            &std::fs::read_to_string(base.join("topic.json"))
                .with_context(|| format!("reading topics/{name}/topic.json"))?,
        )
        .with_context(|| format!("parsing topics/{name}/topic.json"))?;
        let mut spec: TopicSpec = serde_json::from_value(
            resolve_refs(raw_topic, &resolved_macros, &sanitizers)
                .with_context(|| format!("resolving topics/{name}/topic.json"))?,
        )
        .with_context(|| format!("parsing topics/{name}/topic.json"))?;
        spec.geometry.validate().with_context(|| format!("topics/{name}/topic.json: geometry"))?;
        let exclude_condition = spec.exclude_condition.take();

        // Load the named producer library. Optional: a topic with no named producers (e.g.
        // barrierLines) may omit the file. Any `{ "shared": "<name>" }` reference is inlined
        // against the config-root-level shared table (`<config_root>/producers.json`, e.g. the
        // `road` classifier) as raw JSON first, then macro/sanitizer-resolved the same way
        // `topic.json` is, before `Producer` deserialization ever runs — so `Producer` itself never
        // represents any of the three kinds of named reference.
        let shared_producers = load_shared_producers(&config_root)?;
        let producers_path = base.join("producers.json");
        let producer_lib: HashMap<String, Producer> = if producers_path.exists() {
            let raw: Value = serde_json::from_str(&std::fs::read_to_string(&producers_path)?)
                .with_context(|| format!("parsing topics/{name}/producers.json"))?;
            let inlined = inline_shared_producers(raw, &shared_producers)
                .with_context(|| format!("topics/{name}/producers.json: inlining shared producers"))?;
            let resolved = resolve_refs(inlined, &resolved_macros, &sanitizers)
                .with_context(|| format!("resolving topics/{name}/producers.json"))?;
            serde_json::from_value(resolved)
                .with_context(|| format!("parsing topics/{name}/producers.json"))?
        } else {
            HashMap::new()
        };

        // Load per-kind category sets from topics/<name>/{node,way,relation}/, each already fully
        // macro/sanitizer-resolved by `load_topic_categories`, then compile the exclude relation
        // into a priority order + discrimination tree via `build_order`.
        let categories_loaded = load_topic_categories(&base, &resolved_macros, &macros, &sanitizers)
            .with_context(|| format!("loading topics/{name}/ categories"))?;
        let mut categories: HashMap<ElementKind, CategoriesFile> = HashMap::new();
        for (kind, mut cats) in categories_loaded {
            cats.build_order(tree_max_depth)
                .with_context(|| format!("building category order for topics/{name}"))?;
            categories.insert(kind, cats);
        }

        // `accept_all` kinds skip category matching entirely — reject a kind that also has a
        // category directory, since first-match-or-accept-all-fallback would be ambiguous about
        // which one actually governs.
        let accept_all: std::collections::HashSet<ElementKind> =
            ElementKind::ALL.into_iter().filter(|k| spec.accept_all.get(*k)).collect();
        for kind in &accept_all {
            anyhow::ensure!(
                !categories.contains_key(kind),
                "topics/{name}: kind '{}' has both a category directory and accept_all",
                kind.subdir()
            );
        }

        // `transforms.json`, if the topic has one, is the whole per-kind pipeline set (already
        // resolved by `load_topic_transforms`); a topic with none simply has no pipelines (see
        // `TransformsSpec`'s own doc).
        let pipelines = load_topic_transforms(&base, &resolved_macros, &sanitizers)?
            .map(TransformsSpec::into_pipelines)
            .unwrap_or_default();

        // `true` here means "pass every tag through verbatim" (see `ProducersSpec`) — read before
        // `spec.producers` is consumed into a plain fields map below, since it isn't itself a `Field`
        // shape `resolve_producers` can produce. Same flag a bare `null` entry in `passthrough_tags`
        // sets (see `pass_through_remaining_tags`'s own doc for why one flag covers both) — setting
        // both is redundant, not conflicting, so no error either way.
        let mut pass_through_remaining_tags = spec.producers.is_all();
        let mut topic_producers = spec.producers.clone().into_fields_map();

        // `passthrough_tags` is sugar for a batch of `"<tag>": true` producers entries (see
        // `TopicSpec::passthrough_tags`) — folded in here, before any resolving, so the rest of
        // this function never needs to know the two shapes exist. A bare `null` entry has no name
        // to fold under, so it sets `pass_through_remaining_tags` directly instead.
        pass_through_remaining_tags |= spec.passthrough_tags.iter().any(Option::is_none);
        for tag in spec.passthrough_tags.iter().flatten() {
            anyhow::ensure!(
                topic_producers.insert(tag.clone(), Value::Bool(true)).is_none(),
                "topics/{name}/topic.json: '{tag}' is in both passthrough_tags and producers",
            );
        }

        // Every bare-`true` entry at this point (inline `"<tag>": true` or folded-in
        // `passthrough_tags`) is a passthrough, not an authored producer — captured now, before
        // `resolve_producers` turns it into a `Producer::Extract` indistinguishable from a real one.
        let passthrough_producers: std::collections::HashSet<String> =
            topic_producers.iter().filter(|(_, v)| **v == Value::Bool(true)).map(|(k, _)| k.clone()).collect();

        // Topic-default producers, topic-level `defaults` folded in — the defensive fallback for a
        // category id missing from `category_producers` (shouldn't normally happen; see below).
        let mut default_producers = merge_default_fields(
            resolve_producers(
                topic_producers.clone(), &producer_lib, &sanitizers,
                &format!("topics/{name}/topic.json: producers"),
            )?,
            &spec.defaults,
        );

        // Precompute per-category effective producers (topic `producers` ⊕ category `producers`,
        // merged by key before resolving — see `TopicSpec::producers`) and effective `defaults`
        // folded in (`merge_default_fields`), across every kind. Every category gets an entry,
        // even with no `producers` override: its effective defaults can still differ from the
        // topic's. Category ids are expected unique within a topic (they're file stems); a node
        // and a way category sharing a stem would collide here — keep stems distinct per topic.
        let mut category_producers = HashMap::new();
        for cats in categories.values() {
            for cat in &cats.categories {
                let raw = merge(&topic_producers, &cat.producers);
                let fields = resolve_producers(
                    raw, &producer_lib, &sanitizers,
                    &format!("topics/{name}: category '{}' producers", cat.id),
                )?;
                let defaults = merge(&spec.defaults, &cat.defaults);
                category_producers.insert(cat.id.clone(), merge_default_fields(fields, &defaults));
            }
        }

        // Compile a discrimination net into any `Producer::Match` with enough rules to be worth it
        // (see `Producer::compile_trees`/`MATCH_TREE_MIN_RULES`) — every field's producer is fully
        // macro/sanitizer/shared-reference-resolved by now, so this is safe to run once, here.
        //
        // In parallel: each field's producer is compiled independently of every other, and there are
        // hundreds of them (one per output per category — `configs/tilda`'s bikelanes alone has 26
        // categories over ~48 producers). Tree building is otherwise pure startup latency paid
        // before the first blob is read, which on a city-sized extract exceeded the entire
        // classification it speeds up.
        let mut fields: Vec<&mut Field> =
            default_producers.iter_mut().chain(category_producers.values_mut().flatten()).collect();
        fields.par_iter_mut().for_each(|field| field.source.compile_trees(tree_max_depth));

        let mut runner = Self {
            spec,
            categories,
            pipelines,
            exclude_condition,
            default_producers,
            linear_classify: tree_max_depth == 0,
            category_producers,
            pass_through_remaining_tags,
            accept_all,
            passthrough_producers,
            skip_untagged: std::collections::HashSet::new(),
            stamps_side: std::collections::HashSet::new(),
        };

        runner.stamps_side = runner
            .pipelines
            .iter()
            .filter(|(_, steps)| {
                steps.iter().any(|step| match step {
                    TransformStep::Clone(c) => c.annotate.iter().any(|(k, _)| k == "_side"),
                    TransformStep::Transform(_) => false,
                })
            })
            .map(|(&kind, _)| kind)
            .collect();

        // A side object's id is `"{parent_id}/{id_suffix}"` — not derivable from `osm_type`/`osm_id`,
        // since several rows share one `osm_id`. Dropping the column there would lose the only thing
        // distinguishing them, so reject the combination at load time rather than silently emit
        // colliding rows.
        anyhow::ensure!(
            runner.spec.id_type.emits_column() || runner.stamps_side.is_empty(),
            "topic '{}' sets \"id_type\": \"none\" but side-splits ({:?}): a side object's id carries \
             an id_suffix, so without the column its rows are indistinguishable",
            runner.spec.table,
            runner.stamps_side,
        );

        runner.skip_untagged = runner.probe_skip_untagged();
        Ok(runner)
    }

    pub fn table(&self) -> &str {
        &self.spec.table
    }

    /// Whether this topic processes `kind` at all — either a `topics/<t>/<kind>/` category folder,
    /// or `accept_all` declared for it (mutually exclusive, enforced at load time above). Before
    /// this included `accept_all`, `process()`'s `!self.has_kind(kind)` guard short-circuited an
    /// `accept_all` kind before it ever reached `build_topic_rows` — so `accept_all` silently never
    /// fired for any topic, ever (no config in the repo used it until `configs/live_raw`, which is
    /// how this surfaced).
    pub fn has_kind(&self, kind: ElementKind) -> bool {
        self.categories.contains_key(&kind) || self.accept_all.contains(&kind)
    }

    /// Run the finished pipeline against an empty tag map, once per handled kind, and collect the
    /// kinds that produced nothing — see `skip_untagged`'s doc for why this is a probe of the real
    /// matcher rather than an analysis of filter shapes.
    fn probe_skip_untagged(&self) -> std::collections::HashSet<ElementKind> {
        let empty_tags = RawTags::default();
        let no_meta = WayMeta { timestamp: None, user: None, changeset: None };
        [ElementKind::Node, ElementKind::Way, ElementKind::Relation]
            .into_iter()
            .filter(|&kind| {
                self.has_kind(kind)
                    && build_topic_rows(self, kind, 0, &empty_tags, &no_meta).is_empty()
            })
            .collect()
    }

    /// Whether an element of `kind` carrying no tags can be skipped outright for this topic —
    /// either the topic ignores `kind` entirely, or the empty-tag probe at load time produced no
    /// rows (see `skip_untagged`). Lets a caller avoid even decoding such an element.
    /// Whether `kind`'s base objects should carry `_side: "self"` — see `stamps_side`.
    pub fn stamps_side(&self, kind: ElementKind) -> bool {
        self.stamps_side.contains(&kind)
    }

    pub fn skips_untagged(&self, kind: ElementKind) -> bool {
        !self.has_kind(kind) || self.skip_untagged.contains(&kind)
    }
    /// Whether this topic declared `shape` for `kind` (`topic.json`'s `"geometry"` — see
    /// `GeometrySpec`). Replaces the old per-(kind,shape) accessors (`wants_way_graph`/
    /// `wants_way_linestring`/`wants_relation_linestring`) with one generalized lookup, now that
    /// `node`/`way`/`relation` all share the same `GeometryShape` vocabulary.
    pub fn wants(&self, kind: ElementKind, shape: GeometryShape) -> bool {
        match kind {
            ElementKind::Node => self.spec.geometry.node.contains(&shape),
            ElementKind::Way => self.spec.geometry.way.contains(&shape),
            ElementKind::Relation => self.spec.geometry.relation.contains(&shape),
        }
    }

    /// This topic wants a per-topic `{table}_edge` pgRouting table (see `db::topic_edges`) —
    /// shorthand for `wants(Way, Graph)`, the one shape lookup common enough to keep a name.
    pub fn wants_way_graph(&self) -> bool {
        self.wants(ElementKind::Way, GeometryShape::Graph)
    }

    /// Run the topic's pipeline for one element of `kind`, handing off to `build_topic_rows`, which
    /// applies `kind`'s own transform pipeline (if any), `exclude_condition`, side-split,
    /// categorize/extract into tag rows against the kind's category set. `raw_tags` are the
    /// element's untouched tags — `build_topic_rows` only clones them if it actually needs to
    /// mutate a copy (an excluded element, or one with no transform pipeline, never pays for it).
    pub fn process<'a>(
        &self,
        kind: ElementKind,
        osm_id: i64,
        raw_tags: &'a RawTags<'a>,
        meta: &WayMeta,
    ) -> Vec<TopicRow> {
        if !self.has_kind(kind) {
            return Vec::new();
        }
        build_topic_rows(self, kind, osm_id, raw_tags, meta)
    }
}

#[cfg(test)]
mod producer_tree_tests {
    use std::collections::{BTreeMap, BTreeSet};

    use serde_json::Map;

    use crate::categorize::linter::{filter_to_expr, to_nnf, Expr, Literal, Predicate};
    use crate::lang::producer::{ExtractCtx, Producer, Rule};
    use crate::osm::types::RawTags;
    use crate::topic::TopicRunner;

    /// Positive `Eq` atoms in `e` → tag → observed values (mirrors
    /// `decision_tree::tests::collect_pairs`, duplicated here to avoid depending on that module's
    /// private test helper).
    fn collect_pairs(e: &Expr, out: &mut BTreeMap<String, BTreeSet<String>>) {
        match e {
            Expr::Lit(Literal::Pos(Predicate::Eq(extract, v))) => {
                for k in extract.tag_names() {
                    out.entry(k).or_default().insert(v.clone());
                }
            }
            Expr::Lit(_) | Expr::True | Expr::False => {}
            Expr::Not(x) => collect_pairs(x, out),
            Expr::And(xs) | Expr::Or(xs) => xs.iter().for_each(|x| collect_pairs(x, out)),
        }
    }

    /// Every `Match`'s `rules` that got a compiled tree, anywhere inside `p` (a rule's own `value`,
    /// or `Parent`'s inner producer, can itself be a further `Match`).
    fn find_compiled_matches<'a>(p: &'a Producer, out: &mut Vec<&'a [Rule]>) {
        match p {
            Producer::Match { rules, tree, .. } => {
                if tree.is_some() {
                    out.push(rules);
                }
                for r in rules {
                    find_compiled_matches(&r.value, out);
                }
            }
            Producer::Parent(inner) => find_compiled_matches(inner, out),
            Producer::Extract { .. } | Producer::Const { .. } => {}
        }
    }

    /// A compiled `Producer::Match` tree must produce the exact same `Produced` (value + annotate)
    /// as a plain linear scan over the same rules, for every object — the tree only prunes
    /// candidates it can prove don't apply (see `decision_tree::build`'s own doc on why
    /// `Producer::Match` needs `assume_match_is_final: false`).
    #[test]
    fn producer_tree_matches_linear() {
        let runner = TopicRunner::load("roads", crate::config::DEFAULT_TREE_MAX_DEPTH)
            .expect("load roads topic");

        let mut checked_any_field = false;
        for field in runner.default_producers.iter().chain(runner.category_producers.values().flatten()) {
            let mut compiled = Vec::new();
            find_compiled_matches(&field.source, &mut compiled);
            if compiled.is_empty() {
                continue;
            }
            checked_any_field = true;

            let mut refs: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
            for rules in &compiled {
                for r in rules.iter() {
                    collect_pairs(&to_nnf(filter_to_expr(&r.when)), &mut refs);
                }
            }

            let mut linear = field.source.clone();
            linear.clear_trees();

            // Cartesian product over every referenced tag's observed values plus "absent" — small
            // per-tag domains here (a handful of values each), so this stays bounded.
            let keys: Vec<&String> = refs.keys().collect();
            let mut combos: Vec<Vec<Option<&str>>> = vec![vec![]];
            for k in &keys {
                let mut vals: Vec<Option<&str>> = refs[*k].iter().map(|v| Some(v.as_str())).collect();
                vals.push(None);
                let mut next = Vec::with_capacity(combos.len() * vals.len());
                for combo in &combos {
                    for v in &vals {
                        let mut c = combo.clone();
                        c.push(*v);
                        next.push(c);
                    }
                }
                combos = next;
            }

            let mut checked = 0usize;
            for has_parent in [false, true] {
                for combo in &combos {
                    let mut tags: RawTags = RawTags::default();
                    for (k, v) in keys.iter().zip(combo.iter()) {
                        if let Some(v) = v {
                            tags.insert((*k).clone().into(), v.to_string().into());
                        }
                    }
                    let parent_tags = if has_parent { Some(tags.clone()) } else { None };
                    let annotations = Map::new();
                    let ctx = ExtractCtx {
                        obj_tags: &tags,
                        parent_tags: parent_tags.as_ref(),
                        id: "",
                        annotations: &annotations,
                    };
                    let a = field.source.eval(&ctx).map(|p| (p.value, p.annotate));
                    let b = linear.eval(&ctx).map(|p| (p.value, p.annotate));
                    assert_eq!(
                        a, b,
                        "[{}] tree≠linear for tags={tags:?} has_parent={has_parent}",
                        field.output
                    );
                    checked += 1;
                }
            }
            assert!(checked > 0, "[{}] no test cases generated", field.output);
        }
        assert!(checked_any_field, "no compiled Producer::Match trees found to test against");
    }
}

#[cfg(test)]
mod skip_untagged_tests {
    use crate::osm::types::ElementKind;
    use crate::topic::TopicRunner;

    /// Whichever config the process-global root happens to point at — these tests mutate a loaded
    /// runner rather than loading a specific config, because `paths::CONFIG_ROOT` is a `OnceLock`
    /// (first caller wins) and the test binary runs every test in one process, so a test that set
    /// its own root would silently get whichever config another test set first.
    fn any_runner() -> TopicRunner {
        let runners = TopicRunner::load_all(6).expect("loading topics");
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
            !r.probe_skip_untagged().contains(&ElementKind::Node),
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
            serde_json::from_value(serde_json::json!({ "not": { "tag": "railway", "exists": true } }))
                .expect("filter parses"),
        );
        assert!(r.probe_skip_untagged().contains(&ElementKind::Node));
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
        for r in TopicRunner::load_all(6).expect("loading topics") {
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
