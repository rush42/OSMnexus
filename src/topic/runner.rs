use std::collections::HashMap;

use anyhow::Context;
use serde_json::{Map, Value};

use crate::categorize::categories::CategoriesFile;
use crate::lang::filter::Filter;
use crate::lang::producer::Producer;
use crate::categorize::transform::TransformStep;
use crate::topic::load::{
    inline_shared_producers, load_shared_macros, load_shared_producers, load_topic_categories,
    load_topic_macros, load_topic_sanitizers, load_topic_transforms, merge, resolve_macros, resolve_refs,
};
use crate::topic::pipeline::build_topic_rows;
use crate::topic::spec::{resolve_output_entry, Field, GeometryShape, TopicSpec, TransformsSpec};
use crate::osm::types::{ElementKind, RawTags};
use crate::output::rows::TopicRow;
use crate::output::types::OsmMeta;

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
    /// Topic-default fields (`spec.outputs`, resolved, with topic-level `defaults` folded in as
    /// each default's lowest-priority `Fallback` branch — see `merge_default_fields`) — the
    /// fallback used if a category id somehow isn't in `category_outputs` (shouldn't normally
    /// happen: every category gets its own entry at load time below).
    pub default_outputs: Vec<Field>,
    /// Per-category effective outputs: the topic's `outputs` map merged with the category's own
    /// `outputs` overrides (category wins, by key — plain JSON-object merge, see `TopicSpec::outputs`),
    /// then resolved into `Producer`s, with the category's effective (topic ⊕ category) `defaults`
    /// folded in as a trailing fallback branch on each default's output — so a default
    /// is just the lowest-priority producer for its key, evaluated by the same `eval_fields` pass
    /// as any other output. Present for every category (unlike a plain override map, every
    /// category's effective defaults can still differ from the topic's even with no `outputs`
    /// override).
    pub category_outputs: HashMap<String, Vec<Field>>,
    /// Per-output-field timing buckets (`TILDA_PROFILE=1` only), pre-built once from every field
    /// name this topic can ever produce — see `profiling::FieldStages`'s own doc for why it isn't
    /// built lazily during the parallel run.
    pub field_stages: crate::profiling::FieldStages,
}

/// Resolve one topic's or category's raw `outputs` map (already merged by key, category winning)
/// into a `Vec<Field>` — the actual payoff of keying `outputs` by name: no more `apply_overrides`/
/// `resolve_bindings`/`check_unique_outputs` Vec-scanning, just one merge then one resolve pass.
/// Duplicate keys can't arise (JSON object keys are inherently unique per map), so no separate
/// uniqueness check is needed either.
fn resolve_outputs(
    raw: Map<String, Value>,
    producer_lib: &HashMap<String, Producer>,
    sanitizers: &HashMap<String, crate::lang::sanitize::Sanitizer>,
    context: &str,
) -> anyhow::Result<Vec<Field>> {
    raw.into_iter()
        .map(|(output, value)| {
            let source = resolve_output_entry(&output, value, producer_lib, sanitizers)
                .with_context(|| context.to_owned())?;
            Ok(Field { output, source })
        })
        .collect()
}

/// A `defaults` JSON entry as a `Producer`: a bundled `{ "value": ..., "annotate": {...} }` object
/// carries its companions as the producer's own `annotate` (so `Producer::eval` emits them exactly
/// when this branch produces — no separate "did the default survive" bookkeeping needed
/// elsewhere); any other JSON is a bare literal with no companions. `rules: []` means `Match`
/// always falls through to `default`, so this unconditionally "produces" — the default value is
/// only ever *reached* via `Fallback` when nothing higher-priority did. Built directly as a
/// `Producer` value (not JSON) after `resolve_outputs` has already run, so it deliberately bypasses
/// `Producer::resolve` — fine here since it carries no macro/sanitizer references to resolve.
fn default_value_producer(v: &Value) -> Producer {
    let (value, annotate) = match v {
        Value::Object(obj) if obj.contains_key("value") => {
            (obj["value"].clone(), obj.get("annotate").and_then(Value::as_object).cloned().unwrap_or_default())
        }
        _ => (v.clone(), Map::new()),
    };
    Producer::Match {
        rules: Vec::new(),
        default: Some(value),
        annotate,
        origin: crate::lang::producer::MatchOrigin::Default,
        tree: None,
    }
}

/// Wrap `primary`/`default_source` as an unconditional (`when: true`) two-rule `Match` — the
/// `Fallback`-shaped result `resolve()` would produce from `{ "fallback": [primary, default] }`,
/// built directly since this runs after `resolve_outputs` already resolved both producers (a
/// second `resolve` pass isn't needed, and `Producer::Fallback` itself is JSON-parse sugar only —
/// see `Producer::eval`).
fn as_fallback_pair(primary: Producer, default_source: Producer) -> Producer {
    let rule = |value: Producer| crate::lang::producer::Rule { when: Filter::Bool(true), value };
    Producer::Match {
        rules: vec![rule(primary), rule(default_source)],
        default: None,
        annotate: Map::new(),
        origin: crate::lang::producer::MatchOrigin::Fallback,
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

    /// Load a topic from its directory `<config_root>/<name>/`.
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
        let resolved_macros = resolve_macros(&raw_macros, &sanitizers)
            .with_context(|| format!("resolving macros for topics/{name}"))?;
        // The same table, deserialized to `Filter` — used only to seed each `CategoriesFile`'s
        // `macros` (whose `build_order` `Skip`-sink `excludes` entries name a macro directly, not
        // through a condition, so they're never touched by JSON-level macro inlining).
        let macros: HashMap<String, Filter> = resolved_macros.iter()
            .map(|(k, v)| Ok((k.clone(), serde_json::from_value(v.clone())?)))
            .collect::<anyhow::Result<_>>()
            .with_context(|| format!("parsing resolved macros for topics/{name}"))?;

        // `topic.json`, fully macro/sanitizer-resolved before it's ever deserialized into
        // `TopicSpec` — so `exclude_condition` lands as a plain `Option<Filter>` and `outputs`'/
        // `defaults`' values (still untyped `Value`s at this level) already carry no unresolved
        // reference either, letting `resolve_outputs` below skip a further resolve pass.
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

        // `transforms.json`, if the topic has one, is the whole per-kind pipeline set (already
        // resolved by `load_topic_transforms`); a topic with none simply has no pipelines (see
        // `TransformsSpec`'s own doc).
        let pipelines = load_topic_transforms(&base, &resolved_macros, &sanitizers)?
            .map(TransformsSpec::into_pipelines)
            .unwrap_or_default();

        // Topic-default outputs, topic-level `defaults` folded in — the defensive fallback for a
        // category id missing from `category_outputs` (shouldn't normally happen; see below).
        let mut default_outputs = merge_default_fields(
            resolve_outputs(
                spec.outputs.clone(), &producer_lib, &sanitizers,
                &format!("topics/{name}/topic.json: outputs"),
            )?,
            &spec.defaults,
        );

        // Precompute per-category effective outputs (topic `outputs` ⊕ category `outputs`,
        // merged by key before resolving — see `TopicSpec::outputs`) and effective `defaults`
        // folded in (`merge_default_fields`), across every kind. Every category gets an entry,
        // even with no `outputs` override: its effective defaults can still differ from the
        // topic's. Category ids are expected unique within a topic (they're file stems); a node
        // and a way category sharing a stem would collide here — keep stems distinct per topic.
        let mut category_outputs = HashMap::new();
        for cats in categories.values() {
            for cat in &cats.categories {
                let raw = merge(&spec.outputs, &cat.outputs);
                let fields = resolve_outputs(
                    raw, &producer_lib, &sanitizers,
                    &format!("topics/{name}: category '{}' outputs", cat.id),
                )?;
                let defaults = merge(&spec.defaults, &cat.defaults);
                category_outputs.insert(cat.id.to_string(), merge_default_fields(fields, &defaults));
            }
        }

        // Compile a discrimination net into any `Producer::Match` with enough rules to be worth it
        // (see `Producer::compile_trees`/`MATCH_TREE_MIN_RULES`) — every field's producer is fully
        // macro/sanitizer/shared-reference-resolved by now, so this is safe to run once, here.
        for field in default_outputs.iter_mut().chain(category_outputs.values_mut().flatten()) {
            field.source.compile_trees(tree_max_depth);
        }

        let field_stages = crate::profiling::FieldStages::build(
            default_outputs.iter()
                .chain(category_outputs.values().flatten())
                .map(|f| f.output.as_str()),
        );

        Ok(Self {
            spec,
            categories,
            pipelines,
            exclude_condition,
            default_outputs,
            category_outputs,
            field_stages,
        })
    }

    pub fn table(&self) -> &str {
        &self.spec.table
    }

    /// Whether this topic has any categories for `kind` (i.e. a `topics/<t>/<kind>/` folder).
    pub fn has_kind(&self, kind: ElementKind) -> bool {
        self.categories.contains_key(&kind)
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
    pub fn process(
        &self,
        kind: ElementKind,
        osm_id: i64,
        raw_tags: &RawTags,
        meta: &OsmMeta,
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
        for field in runner.default_outputs.iter().chain(runner.category_outputs.values().flatten()) {
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
                            tags.insert((*k).clone(), v.to_string());
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
