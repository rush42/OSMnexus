use std::collections::HashMap;

use anyhow::Context;
use serde_json::{Map, Value};

use crate::tag_engine::categories::CategoriesFile;
use crate::tag_engine::extract::Extract;
use crate::tag_engine::filter::Filter;
use crate::tag_engine::input_transforms::InputTransform;
use crate::tag_engine::producer::{Producer, TagSet};
use crate::tag_engine::transform::side_split::CenterLineTransformation;
use crate::topic::load::{
    inline_shared_producers, load_shared_macros, load_shared_producers, load_topic_categories,
    load_topic_macros, load_topic_sanitizers, merge,
};
use crate::topic::pipeline::build_topic_rows;
use crate::topic::spec::{resolve_output_entry, Field, GeometryShape, InputTransformSpec, SplitSidesSpec, TopicSpec};
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
    /// In-place tag mutations applied to each way's tags, in declared order, split around
    /// `exclude_condition` at `exclude_check_at` — so, unlike an output's producer, these can
    /// influence which category a way matches (or whether `exclude_condition` excludes it at all).
    pub input_transforms: Vec<InputTransform>,
    /// Index into `input_transforms` where `exclude_condition` is evaluated: `input_transforms[..n]` run
    /// first, then `exclude_condition`, then `input_transforms[n..]`. Set to the first `UnnestTags`
    /// step's index (or `input_transforms.len()` if there is none) — mirrors the original two-stage
    /// pipeline, where tag rewrites ran before `exclude_condition` but unnesting (which can promote
    /// a `cycleway:access=no`-style tag onto a bare `access` that `exclude_condition` checks
    /// directly) always ran after it.
    pub exclude_check_at: usize,
    /// Center-line side split (from a `split_sides` entry); empty if the topic has none. Applied
    /// after every `InputTransform`, since it changes object cardinality rather than mutating tags.
    pub transformations: Vec<CenterLineTransformation>,
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
}

/// Resolve one topic's or category's raw `outputs` map (already merged by key, category winning)
/// into a `Vec<Field>` — the actual payoff of keying `outputs` by name: no more `apply_overrides`/
/// `resolve_bindings`/`check_unique_outputs` Vec-scanning, just one merge then one resolve pass.
/// Duplicate keys can't arise (JSON object keys are inherently unique per map), so no separate
/// uniqueness check is needed either.
fn resolve_outputs(
    raw: Map<String, Value>,
    producer_lib: &HashMap<String, Producer>,
    macros: &HashMap<String, Filter>,
    sanitizers: &HashMap<String, crate::tag_engine::sanitize::Sanitizer>,
    context: &str,
) -> anyhow::Result<Vec<Field>> {
    raw.into_iter()
        .map(|(output, value)| {
            let mut field = resolve_output_entry(&output, value, producer_lib)
                .with_context(|| context.to_owned())?;
            field.source = field.source.resolve(macros, sanitizers)
                .with_context(|| format!("{context}.{output}"))?;
            Ok(field)
        })
        .collect()
}

/// A `defaults` JSON entry as a `Producer`: a bundled `{ "value": ..., "consts": {...} }` object
/// carries its companions as the producer's own `consts` (so `Producer::eval` emits them exactly
/// when this branch produces — no separate "did the default survive" bookkeeping needed
/// elsewhere); any other JSON is a bare literal with no companions. `rules: []` means `Match`
/// always falls through to `default`, so this unconditionally "produces" — the default value is
/// only ever *reached* via `Fallback` when nothing higher-priority did. Built directly as a
/// `Producer` value (not JSON) after `resolve_outputs` has already run, so it deliberately bypasses
/// `Producer::resolve` — fine here since it carries no macro/sanitizer references to resolve.
fn default_value_producer(v: &Value) -> Producer {
    let (value, consts) = match v {
        Value::Object(obj) if obj.contains_key("value") => {
            (obj["value"].clone(), obj.get("consts").and_then(Value::as_object).cloned().unwrap_or_default())
        }
        _ => (v.clone(), Map::new()),
    };
    Producer::Match { rules: Vec::new(), default: Some(value), consts }
}

/// Wrap `primary`/`default_source` as an unconditional (`when: true`) two-rule `Match` — the
/// `Fallback`-shaped result `resolve()` would produce from `{ "fallback": [primary, default] }`,
/// built directly since this runs after `resolve_outputs` already resolved both producers (a
/// second `resolve` pass isn't needed, and `Producer::Fallback` itself is JSON-parse sugar only —
/// see `Producer::eval`).
fn as_fallback_pair(primary: Producer, default_source: Producer) -> Producer {
    let rule = |p: Producer| crate::tag_engine::classifier::Rule {
        when: Filter::Bool(true),
        value: crate::tag_engine::classifier::ValueSpec::Producer(Box::new(p)),
    };
    Producer::Match {
        rules: vec![rule(primary), rule(default_source)],
        default: None,
        consts: Map::new(),
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

        let mut spec: TopicSpec = serde_json::from_str(
            &std::fs::read_to_string(base.join("topic.json"))
                .with_context(|| format!("reading topics/{name}/topic.json"))?,
        )
        .with_context(|| format!("parsing topics/{name}/topic.json"))?;

        // Named atomic transforms (`sanitize:` targets), shared+topic-local. A separate
        // registry/namespace from the producer library below — atomic chains and composite
        // producers are different *types* (`Sanitizer`/`Producer`), so there's no risk of a
        // name meaning two things at once. Loaded before macros, since a macro's own condition can
        // carry a `sanitize:` too.
        let sanitizers = load_topic_sanitizers(&base, &config_root)?;

        // Every macro this topic can reference: shared (config root's macros.json) plus the
        // topic's own macros.json, topic-local winning on name conflict. Expanded once, here,
        // against itself (so a macro referencing another macro resolves too) — every
        // `Filter`/`Producer` this topic owns is then expanded against the *expanded* result
        // below, so `eval` never does a live macro lookup and an unknown/cyclic macro is a
        // load-time error, not a per-object runtime no-op (see `Filter::expand`).
        let shared_macros = load_shared_macros(&config_root).with_context(|| "loading shared macros.json")?;
        let topic_macros = load_topic_macros(&base)?;
        let raw_macros = merge(&shared_macros, &topic_macros);
        let macros: HashMap<String, Filter> = raw_macros.iter()
            .map(|(k, v)| Ok((k.clone(), v.expand(&raw_macros, &sanitizers)?)))
            .collect::<anyhow::Result<_>>()
            .with_context(|| format!("expanding macros for topics/{name}"))?;

        if let Some(cond) = spec.exclude_condition.take() {
            spec.exclude_condition = Some(cond.expand(&macros, &sanitizers)
                .with_context(|| format!("topics/{name}/topic.json: exclude_condition"))?);
        }

        // Load the named producer library. Optional: a topic with no named producers (e.g.
        // barrierLines) may omit the file. Any `{ "shared": "<name>" }` reference is inlined
        // against the config-root-level shared table (`<config_root>/producers.json`, e.g. the
        // `road` classifier) as raw JSON before `Producer` deserialization ever runs — the same
        // shared→topic-local treatment `load_topic_sanitizers`/`load_shared_macros` give
        // sanitizers/macros — so `Producer` itself never represents "a shared classifier".
        let shared_producers = load_shared_producers(&config_root)?;
        let producers_path = base.join("producers.json");
        let producer_lib: HashMap<String, Producer> = if producers_path.exists() {
            let raw: Value = serde_json::from_str(&std::fs::read_to_string(&producers_path)?)
                .with_context(|| format!("parsing topics/{name}/producers.json"))?;
            let inlined = inline_shared_producers(raw, &shared_producers)
                .with_context(|| format!("topics/{name}/producers.json: inlining shared producers"))?;
            serde_json::from_value(inlined)
                .with_context(|| format!("parsing topics/{name}/producers.json"))?
        } else {
            HashMap::new()
        };

        // Resolve every producer's embedded macros/sanitizers references now, before anything
        // downstream (category `outputs` overrides) clones entries out of this map — every clone
        // then inherits the resolution for free.
        let producer_lib: HashMap<String, Producer> = producer_lib.into_iter()
            .map(|(k, v)| {
                let resolved = v.resolve(&macros, &sanitizers)
                    .with_context(|| format!("topics/{name}: producer '{k}'"))?;
                Ok((k, resolved))
            })
            .collect::<anyhow::Result<_>>()?;

        // Load per-kind category sets from topics/<name>/{node,way,relation}/.
        let mut categories = load_topic_categories(&base)
            .with_context(|| format!("loading topics/{name}/ categories"))?;

        // Every kind shares the one already-expanded macro map, and every category condition is
        // expanded against it — `build_order`'s `Skip` sink conditions (`self.macros.get(name)
        // .cloned()`) then pick up already-expanded `Filter`s for free.
        for cats in categories.values_mut() {
            cats.macros = macros.clone();
            for cat in &mut cats.categories {
                cat.condition = cat.condition.expand(&macros, &sanitizers)
                    .with_context(|| format!("topics/{name}: category '{}'", cat.id))?;
            }
            // Compile the exclude relation into a priority order + discrimination tree (needs macros
            // fully merged first). categorize() is then pure first-match over this — no runtime excludes.
            cats.build_order(tree_max_depth)
                .with_context(|| format!("building category order for topics/{name}"))?;
        }

        // Center-line splits: change object cardinality, so handled separately from `input_transforms`
        // and always applied last (see `SplitSidesSpec`).
        let mut transformations = Vec::new();
        for s in &spec.split_sides {
            let SplitSidesSpec { highway, prefix, directed_keys, self_directed_keys } = s;
            // `directed_keys`/`self_directed_keys` are just key names in JSON — translated here
            // into ordinary `InputTransform`s (a `directed` `Producer::Extract` per key), applied
            // per side object post-split (see `topic::pipeline`). The split itself never sees
            // these; it only unnests.
            let directed_step = |key: &String, from: TagSet| InputTransform::TagRule {
                output: key.clone(),
                source: Producer::DirectedExtract { key: key.clone(), from, sanitize: None, consts: Map::new() },
            };
            let steps: Vec<InputTransform> = directed_keys.iter().map(|k| directed_step(k, TagSet::Parent))
                .chain(self_directed_keys.iter().map(|k| directed_step(k, TagSet::Obj)))
                .collect();
            transformations.push(CenterLineTransformation {
                highway: Box::leak(highway.clone().into_boxed_str()),
                prefix:  Box::leak(prefix.clone().into_boxed_str()),
                directed_steps: Box::leak(steps.into_boxed_slice()),
            });
        }

        // Split the declared input-transform pipeline into one ordered list of in-place
        // `InputTransform`s, applied in declaration order before `exclude_condition`/categorization.
        let mut input_transforms = Vec::new();
        for t in &spec.input_transforms {
            match t {
                InputTransformSpec::UnnestSidepathSelf { prefix } => {
                    let prefix = Box::leak(prefix.clone().into_boxed_str()) as &'static str;
                    input_transforms.push(InputTransform::UnnestTags {
                        prefix,
                        infix: "",
                        meta_prefixes: crate::tag_engine::transform::side_split::META_PREFIXES,
                        guard: Some(Filter::TagInSet {
                            extract: Extract::Value { key: "highway".to_owned() },
                            sanitize: None,
                            in_set: "sidepath_highway".to_owned(),
                        }),
                    });
                }
                InputTransformSpec::TagRules { output, source } => {
                    input_transforms.push(InputTransform::TagRule {
                        output: output.clone(),
                        source: source.resolve(&macros, &sanitizers)
                            .with_context(|| format!("topics/{name}/topic.json: input_transforms.{output}"))?,
                    });
                }
                InputTransformSpec::StripPrefix {
                    prefix, stamp_key, stamp_value, stamp_nested_under,
                } => {
                    input_transforms.push(InputTransform::StripPrefix {
                        prefix: prefix.clone(),
                        stamp_key: stamp_key.clone(),
                        stamp_value: stamp_value.clone(),
                        stamp_nested_under: stamp_nested_under.clone(),
                    });
                }
            }
        }
        // Only `UnnestSidepathSelf` config entries ever produce an `UnnestTags` step today (no
        // topic.json shape exposes a bare in-place unnest yet), so matching the variant is
        // equivalent to matching the old dedicated `SidepathSelf` one.
        let exclude_check_at = input_transforms
            .iter()
            .position(|s| matches!(s, InputTransform::UnnestTags { .. }))
            .unwrap_or(input_transforms.len());

        // Topic-default outputs, topic-level `defaults` folded in — the defensive fallback for a
        // category id missing from `category_outputs` (shouldn't normally happen; see below).
        let default_outputs = merge_default_fields(
            resolve_outputs(
                spec.outputs.clone(), &producer_lib, &macros, &sanitizers,
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
                    raw, &producer_lib, &macros, &sanitizers,
                    &format!("topics/{name}: category '{}' outputs", cat.id),
                )?;
                let defaults = merge(&spec.defaults, &cat.defaults);
                category_outputs.insert(cat.id.clone(), merge_default_fields(fields, &defaults));
            }
        }

        Ok(Self {
            spec,
            categories,
            input_transforms,
            exclude_check_at,
            transformations,
            default_outputs,
            category_outputs,
        })
    }

    pub fn table(&self) -> &str {
        &self.spec.table
    }

    /// Whether this topic has any categories for `kind` (i.e. a `topics/<t>/<kind>/` folder).
    pub fn has_kind(&self, kind: ElementKind) -> bool {
        self.categories.contains_key(&kind)
    }

    /// This topic wants a per-topic `{table}_edge` pgRouting table (see `db::topic_edges`).
    pub fn wants_way_graph(&self) -> bool {
        self.spec.geometry.way.contains(&GeometryShape::Graph)
    }

    /// This topic wants whole-way linestrings, routed per-way during streaming (see
    /// `db::topic_geometries` / `main.rs`'s `build_geom_cb`).
    pub fn wants_way_linestring(&self) -> bool {
        self.spec.geometry.way.contains(&GeometryShape::Linestring)
    }

    /// This topic wants merged relation linestrings (see `db::topic_geometries`).
    pub fn wants_relation_linestring(&self) -> bool {
        self.spec.geometry.relation.contains(&GeometryShape::Linestring)
    }

    /// Run the topic's pipeline for one element of `kind`: clone its raw tags, then hand off to
    /// `build_topic_rows`, which applies `input_transforms` (way kind only — they're way-oriented),
    /// `exclude_condition`, side-split, categorize/extract into tag rows against the kind's
    /// category set. `raw_tags` are the element's untouched tags. Geometry is produced separately
    /// (way-only) via `build_geom_rows`.
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
        build_topic_rows(self, kind, osm_id, raw_tags.clone(), meta)
    }
}
