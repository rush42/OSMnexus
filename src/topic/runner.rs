use std::collections::HashMap;

use anyhow::Context;
use serde_json::{Map, Value};

use crate::tag_engine::categories::CategoriesFile;
use crate::tag_engine::filter::Filter;
use crate::tag_engine::input_transforms::InputTransform;
use crate::tag_engine::producer::{Producer, TagSet};
use crate::tag_engine::transform::side_split::CenterLineTransformation;
use crate::topic::load::{load_shared_macros, load_topic_categories, load_topic_macros, load_topic_sanitizers, merge};
use crate::topic::pipeline::build_topic_rows;
use crate::topic::spec::{Field, InputTransformSpec, SplitSidesSpec, TopicSpec};
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
    /// `exclude_condition` at `exclude_check_at` — so, unlike a deriver, these can influence which
    /// category a way matches (or whether `exclude_condition` excludes it at all).
    pub input_transforms: Vec<InputTransform>,
    /// Index into `input_transforms` where `exclude_condition` is evaluated: `input_transforms[..n]` run
    /// first, then `exclude_condition`, then `input_transforms[n..]`. Set to the first `SidepathSelf`
    /// step's index (or `input_transforms.len()` if there is none) — mirrors the original two-stage
    /// pipeline, where tag rewrites ran before `exclude_condition` but unnesting (which can promote
    /// a `cycleway:access=no`-style tag onto a bare `access` that `exclude_condition` checks
    /// directly) always ran after it.
    pub exclude_check_at: usize,
    /// Center-line side split (from a `split_sides` entry); empty if the topic has none. Applied
    /// after every `InputTransform`, since it changes object cardinality rather than mutating tags.
    pub transformations: Vec<CenterLineTransformation>,
    /// Desugared `sanitizers`, kept only for the topic-load summary log (`main.rs`) — evaluation
    /// doesn't use this list; it's folded into `topic_derivers` (see there) so sanitizers and
    /// derivers run as one list through one `eval_fields` call.
    pub sanitizer_fields: Vec<Field>,
    /// Topic-default fields applied to every object regardless of category: desugared sanitizers
    /// first, then resolved `derivers.json` bindings (`topic.json`'s `derivers` list) — sanitizers
    /// and derivers are the same `Producer`-evaluation mechanism, just two JSON shorthands for
    /// declaring an entry in this one list. Order matters only if a sanitizer and a deriver ever
    /// target the same output: the deriver (evaluated later) wins.
    pub topic_derivers: Vec<Field>,
    /// Per-category effective derivers: topic defaults, with the category's own deriver
    /// re-bindings applied by output, and the category's effective (topic ⊕ category) consts
    /// folded in as a trailing `Producer::Fallback` branch on each const's output — so a const is
    /// just the lowest-priority producer for its key, evaluated by the same `eval_fields` pass as
    /// any sanitizer/deriver, with no separate seed-then-backfill step needed. Present for every
    /// category (unlike a plain override map, every category's effective consts can differ from
    /// the topic defaults even with no deriver override).
    pub category_derivers: HashMap<String, Vec<Field>>,
    /// Per-category effective *private* consts only (`_`-prefixed keys of topic ⊕ category
    /// `consts` — see `TopicSpec::consts`): nothing ever produces these outside `consts` itself,
    /// so they're seeded into `private` unconditionally rather than folded into a producer chain.
    pub category_private_consts: HashMap<String, serde_json::Map<String, serde_json::Value>>,
}

/// Resolve a list of deriver bindings against the `derivers.json` library, erroring on any
/// dangling reference (load-time validation — the cost of name indirection bought back).
fn resolve_bindings(
    lib: &HashMap<String, Producer>,
    bindings: &[crate::tag_engine::categories::DeriverBinding],
    topic: &str,
) -> anyhow::Result<Vec<Field>> {
    bindings
        .iter()
        .map(|b| {
            let source = lib.get(b.deriver()).cloned().ok_or_else(|| {
                anyhow::anyhow!(
                    "topics/{topic}: deriver '{}' not found in derivers.json",
                    b.deriver()
                )
            })?;
            Ok(Field { output: b.output().to_owned(), source })
        })
        .collect()
}

/// Apply a category's deriver overrides on top of the topic defaults, replacing by output.
fn apply_overrides(base: &[Field], overrides: Vec<Field>) -> Vec<Field> {
    let mut fields = base.to_vec();
    for ov in overrides {
        match fields.iter_mut().find(|f| f.output == ov.output) {
            Some(existing) => existing.source = ov.source,
            None => fields.push(ov),
        }
    }
    fields
}

/// Every field's `output` is unique within `fields` — a config error otherwise: two entries
/// racing for the same output would make the winner an unreadable function of construction
/// order, and `eval_fields` no longer needs an intra-call `written` set to tell them apart when
/// this invariant holds. Called at every point a `Vec<Field>` is assembled from independent
/// sources (topic-level derivers, category overrides, consts) rather than once at the end, so
/// the error names the exact assembly step at fault.
fn check_unique_outputs(fields: &[Field], context: &str) -> anyhow::Result<()> {
    let mut seen = std::collections::HashSet::new();
    for f in fields {
        if !seen.insert(f.output.as_str()) {
            anyhow::bail!("{context}: output '{}' is produced by more than one entry", f.output);
        }
    }
    Ok(())
}

/// A `consts` JSON entry as a `Producer`: a bundled `{ "value": ..., "consts": {...} }` object
/// carries its companions as the producer's own `consts` (so `Producer::eval` emits them exactly
/// when this branch produces — no separate "did the const survive" bookkeeping needed elsewhere);
/// any other JSON is a bare literal with no companions. `rules: []` means `Classify` always falls
/// through to `default`, so this unconditionally "produces" — the const is only ever *reached* via
/// `Fallback` when nothing higher-priority did.
fn const_producer(v: &Value) -> Producer {
    let (value, consts) = match v {
        Value::Object(obj) if obj.contains_key("value") => {
            (obj["value"].clone(), obj.get("consts").and_then(Value::as_object).cloned().unwrap_or_default())
        }
        _ => (v.clone(), Map::new()),
    };
    Producer::Classify { rules: Vec::new(), default: Some(value), from: TagSet::Obj, consts }
}

/// Fold `consts`' public (non-`_`-prefixed) keys into `fields` as the lowest-priority producer for
/// their output: appended as a trailing `Fallback` branch onto an existing field targeting that
/// output (so the const only takes effect when the real producer returns `None`), or pushed as a
/// new const-only field when nothing else targets it (e.g. a bare literal like `minzoom`).
/// `_`-prefixed keys are skipped — see `TopicRunner::category_private_consts`.
fn merge_const_fields(mut fields: Vec<Field>, consts: &Map<String, Value>) -> Vec<Field> {
    for (k, v) in consts {
        if k.starts_with('_') {
            continue;
        }
        let const_source = const_producer(v);
        match fields.iter_mut().find(|f| &f.output == k) {
            Some(existing) => {
                let primary = std::mem::replace(&mut existing.source, Producer::Fallback { fallback: Vec::new() });
                existing.source = Producer::Fallback { fallback: vec![primary, const_source] };
            }
            None => fields.push(Field { output: k.clone(), source: const_source }),
        }
    }
    fields
}

impl TopicRunner {
    /// Discover and load every topic under the active config directory, skipping `_`-prefixed
    /// directories (e.g. `_shared/`). Returned in sorted name order for deterministic output.
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
        let shared_dir = base.parent().expect("topics/<name> has a parent").join("_shared");

        let mut spec: TopicSpec = serde_json::from_str(
            &std::fs::read_to_string(base.join("topic.json"))
                .with_context(|| format!("reading topics/{name}/topic.json"))?,
        )
        .with_context(|| format!("parsing topics/{name}/topic.json"))?;

        // Named atomic transforms (`sanitize:` targets), shared+topic-local. A separate
        // registry/namespace from the deriver library below — atomic chains and composite
        // producers are different *types* (`AtomicChain`/`Producer`), so there's no risk of a
        // name meaning two things at once. Loaded before macros, since a macro's own condition can
        // carry a `sanitize:` too.
        let sanitizers = load_topic_sanitizers(&base, &shared_dir)?;

        // Every macro this topic can reference: shared (topics/_shared/macros/) plus the topic's
        // own macros.json, topic-local winning on name conflict. Expanded once, here, against
        // itself (so a macro referencing another macro resolves too) — every `Filter`/`Producer`
        // this topic owns is then expanded against the *expanded* result below, so `eval` never
        // does a live macro lookup and an unknown/cyclic macro is a load-time error, not a
        // per-object runtime no-op (see `Filter::expand`).
        let shared_macros = load_shared_macros(&shared_dir).with_context(|| "loading topics/_shared/")?;
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
        for f in &mut spec.osm_fields {
            f.source = f.source.resolve(&macros, &sanitizers)
                .with_context(|| format!("topics/{name}/topic.json: osm_fields.{}", f.output))?;
        }

        // Sanitizers already deserialize straight into `Field` (see `Field`'s `Deserialize` impl) —
        // `{tag, name}` is just a terser JSON shorthand for the common single-`Extract`-with-
        // `sanitize` case, the same `Producer`-evaluation path as derivers. Folded into
        // `topic_derivers` below (not kept as a separate list) so a category can override a
        // sanitizer's output exactly the way it overrides a deriver's — same mechanism, no new
        // JSON syntax needed. Its `sanitize:` reference still needs resolving like any other.
        let sanitizer_fields: Vec<Field> = spec.sanitizers.iter()
            .map(|f| Ok(Field {
                output: f.output.clone(),
                source: f.source.resolve(&macros, &sanitizers)
                    .with_context(|| format!("topics/{name}/topic.json: sanitizers.{}", f.output))?,
            }))
            .collect::<anyhow::Result<_>>()?;

        // Load the deriver library (named composite producers). Optional: a topic with no derivers
        // (e.g. barrierLines) may omit the file.
        let derivers_path = base.join("derivers.json");
        let deriver_lib: HashMap<String, Producer> = if derivers_path.exists() {
            serde_json::from_str(&std::fs::read_to_string(&derivers_path)?)
                .with_context(|| format!("parsing topics/{name}/derivers.json"))?
        } else {
            HashMap::new()
        };

        // Resolve every deriver's embedded macros/sanitizers/shared-classifier references now,
        // before anything downstream (`resolve_bindings`, category overrides) clones entries out
        // of this map — every clone then inherits the resolution for free.
        let deriver_lib: HashMap<String, Producer> = deriver_lib.into_iter()
            .map(|(k, v)| {
                let resolved = v.resolve(&macros, &sanitizers)
                    .with_context(|| format!("topics/{name}: deriver '{k}'"))?;
                Ok((k, resolved))
            })
            .collect::<anyhow::Result<_>>()?;

        // Resolve the topic-default deriver bindings (validates references), appended after the
        // sanitizer fields. A sanitizer and a topic-level deriver silently sharing an output would
        // mean the deriver wins with no visible signal *why* the sanitizer never took effect — so
        // that overlap is a config error, not a documented precedence rule. (A *category*
        // overriding a sanitizer's output is fine — that override is explicit by construction:
        // it names the output it's replacing.)
        let topic_derivers_resolved = resolve_bindings(&deriver_lib, &spec.derivers, name)?;
        check_unique_outputs(&topic_derivers_resolved, &format!("topics/{name}/topic.json: derivers"))?;
        if let Some(dup) = topic_derivers_resolved.iter()
            .find(|d| sanitizer_fields.iter().any(|s| s.output == d.output))
        {
            anyhow::bail!(
                "topics/{name}: sanitizer and deriver both target output '{}' — remove one, or \
                 if this is an intentional per-category override, move it into that category's \
                 `derivers` list instead of the topic-level one",
                dup.output
            );
        }
        let mut topic_derivers = sanitizer_fields.clone();
        topic_derivers.extend(topic_derivers_resolved);

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
                source: Producer::Extract {
                    key: Some(key.clone()), keys: None, from, side: None, sanitize: None,
                    consts: Map::new(), directed: true,
                },
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
                    input_transforms.push(InputTransform::SidepathSelf { prefix });
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
        let exclude_check_at = input_transforms
            .iter()
            .position(|s| matches!(s, InputTransform::SidepathSelf { .. }))
            .unwrap_or(input_transforms.len());

        // Precompute per-category effective derivers (topic defaults ± overrides, with the
        // category's effective consts folded in — see `merge_const_fields`) and private consts,
        // across every kind. Every category gets an entry, even with no deriver override: its
        // effective consts can still differ from the topic's. Category ids are expected unique
        // within a topic (they're file stems); a node and a way category sharing a stem would
        // collide here — keep stems distinct per topic.
        let mut category_derivers = HashMap::new();
        let mut category_private_consts = HashMap::new();
        for cats in categories.values() {
            for cat in &cats.categories {
                let base = match &cat.derivers {
                    Some(bindings) => {
                        let overrides = resolve_bindings(&deriver_lib, bindings, name)?;
                        check_unique_outputs(
                            &overrides,
                            &format!("topics/{name}: category '{}' derivers", cat.id),
                        )?;
                        apply_overrides(&topic_derivers, overrides)
                    }
                    None => topic_derivers.clone(),
                };
                let consts = merge(&spec.consts, &cat.consts);
                category_derivers.insert(cat.id.clone(), merge_const_fields(base, &consts));
                category_private_consts.insert(
                    cat.id.clone(),
                    consts.into_iter().filter(|(k, _)| k.starts_with('_')).collect(),
                );
            }
        }

        Ok(Self {
            spec,
            categories,
            input_transforms,
            exclude_check_at,
            transformations,
            sanitizer_fields,
            topic_derivers,
            category_derivers,
            category_private_consts,
        })
    }

    pub fn table(&self) -> &str {
        &self.spec.table
    }

    /// Whether this topic has any categories for `kind` (i.e. a `topics/<t>/<kind>/` folder).
    pub fn has_kind(&self, kind: ElementKind) -> bool {
        self.categories.contains_key(&kind)
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
