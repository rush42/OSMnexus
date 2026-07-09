use std::collections::HashMap;

use anyhow::Context;
use serde_json::{Map, Value};

use crate::tag_engine::categories::{load_shared_macros, load_topic_categories, CategoriesFile};
use crate::tag_engine::sanitize::{SanitizerDef, SanitizerRegistry};
use crate::tag_engine::producer::{ExtractCtx, Producer, TagSet};
use crate::tag_engine::{runner::build_topic_rows, topic::{DeriverBinding, Field, ParamTransform, TopicSpec}};
use crate::osm::types::{ElementKind, RawTags};
use crate::output::rows::TopicRow;
use crate::output::types::OsmMeta;
use crate::tag_engine::transform::side_split::CenterLineTransformation;

/// Per-key overlay: the topic-level default map with the category's entries layered on top.
/// The shared shape behind both `consts` (→ derived) and `private` (→ private column).
fn overlay(
    base: &serde_json::Map<String, serde_json::Value>,
    over: &serde_json::Map<String, serde_json::Value>,
) -> serde_json::Map<String, serde_json::Value> {
    let mut merged = base.clone();
    for (k, v) in over {
        merged.insert(k.clone(), v.clone());
    }
    merged
}

/// One in-place tag mutation, applied to an object's tags before categorization — either at the
/// whole-way, pre-split stage (`obj_side: "self"`, no `parent_tags`), or, for `directed`-style
/// steps, per already-split object (its own resolved side + the parent way's tags). This is the
/// same primitive either way; only the `ExtractCtx` passed to `apply` differs.
#[derive(Clone)]
pub enum PreCatStep {
    /// Write `output` from a full `Producer`. A produced `null` deletes `output`; a produced
    /// non-null value must be a string and overwrites it; no match (`None`) leaves it untouched.
    TagRule { output: String, source: Producer },
    /// Unnest bare `prefix`-prefixed tags onto sidepath-class ways — see
    /// `side_split::apply_sidepath_self`.
    SidepathSelf { prefix: &'static str },
    /// Strip `prefix` from matching keys — see `transform::strip_prefix`. The one step needing
    /// dynamic key iteration, so it isn't a `Producer`.
    StripPrefix {
        prefix: String,
        stamp_key: String,
        stamp_value: String,
        stamp_nested_under: Vec<String>,
    },
}

impl PreCatStep {
    pub fn apply(
        &self,
        tags: &mut RawTags,
        parent_tags: Option<&RawTags>,
        obj_side: &str,
        sanitizers: &SanitizerRegistry,
        derivers: &HashMap<String, Producer>,
    ) {
        match self {
            PreCatStep::TagRule { output, source } => {
                let ctx = ExtractCtx {
                    obj_tags: tags,
                    parent_tags,
                    parking_inference: None,
                    obj_side,
                    sanitizers,
                    derivers,
                };
                if let Some(p) = source.eval(&ctx) {
                    match p.value {
                        Value::Null => { tags.remove(output); }
                        Value::String(s) => { tags.insert(output.clone(), s); }
                        other => panic!(
                            "tag_rules for '{output}' produced a non-string, non-null value: {other}"
                        ),
                    }
                }
            }
            PreCatStep::SidepathSelf { prefix } => {
                crate::tag_engine::transform::side_split::apply_sidepath_self(tags, &[prefix]);
            }
            PreCatStep::StripPrefix { prefix, stamp_key, stamp_value, stamp_nested_under } => {
                crate::tag_engine::transform::strip_prefix(tags, prefix, stamp_key, stamp_value, stamp_nested_under);
            }
        }
    }
}

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
    pub pre_cat_steps: Vec<PreCatStep>,
    /// Index into `pre_cat_steps` where `exclude_condition` is evaluated: `pre_cat_steps[..n]` run
    /// first, then `exclude_condition`, then `pre_cat_steps[n..]`. Set to the first `SidepathSelf`
    /// step's index (or `pre_cat_steps.len()` if there is none) — mirrors the original two-stage
    /// pipeline, where tag rewrites ran before `exclude_condition` but unnesting (which can promote
    /// a `cycleway:access=no`-style tag onto a bare `access` that `exclude_condition` checks
    /// directly) always ran after it.
    pub exclude_check_at: usize,
    /// Center-line side split (from a `split_sides` entry); empty if the topic has none. Applied
    /// after every `PreCatStep`, since it changes object cardinality rather than mutating tags.
    pub transformations: Vec<CenterLineTransformation>,
    /// Desugared `sanitizers`, kept only for the topic-load summary log (`main.rs`) — evaluation
    /// doesn't use this list; it's folded into `topic_derivers` (see there) so sanitizers and
    /// derivers run as one list through one `eval_fields` call.
    pub sanitizer_fields: Vec<Field>,
    /// Data-defined sanitizer chains (sanitizers.json) layered over the built-in registry.
    pub sanitizers: SanitizerRegistry,
    /// The deriver library (derivers.json) — kept so Rust derivers can re-evaluate a sibling
    /// by name (e.g. smoothness_parent re-runs the base smoothness fallback on the parent).
    pub deriver_lib: HashMap<String, Producer>,
    /// Topic-default fields applied to every object regardless of category: desugared sanitizers
    /// first, then resolved `derivers.json` bindings (`topic.json`'s `derivers` list) — sanitizers
    /// and derivers are the same `Producer`-evaluation mechanism, just two JSON shorthands for
    /// declaring an entry in this one list. Order matters only if a sanitizer and a deriver ever
    /// target the same output: the deriver (evaluated later) wins.
    pub topic_derivers: Vec<Field>,
    /// Per-category effective derivers — present only for categories that override a deriver
    /// (topic defaults with the category's re-bindings applied by output). Categories absent
    /// from this map use `topic_derivers` directly.
    pub category_derivers: HashMap<String, Vec<Field>>,
    /// Per-category effective consts (topic-level `consts` overlaid by the category's), seeded
    /// into `derived` as the lowest-priority layer. Present for every category.
    pub category_consts: HashMap<String, serde_json::Map<String, serde_json::Value>>,
    /// Per-category effective private metadata (topic-level `private` overlaid by the category's),
    /// emitted into the `private` column. Present for every category.
    pub category_private: HashMap<String, serde_json::Map<String, serde_json::Value>>,
}

/// Resolve a list of deriver bindings against the `derivers.json` library, erroring on any
/// dangling reference (load-time validation — the cost of name indirection bought back).
fn resolve_bindings(
    lib: &HashMap<String, Producer>,
    bindings: &[DeriverBinding],
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

        let spec: TopicSpec = serde_json::from_str(
            &std::fs::read_to_string(base.join("topic.json"))
                .with_context(|| format!("reading topics/{name}/topic.json"))?,
        )
        .with_context(|| format!("parsing topics/{name}/topic.json"))?;

        // Sanitizers desugar to per-object `Field`s — the same `Producer`-evaluation path as
        // derivers, just a terser JSON shorthand (`{tag, name}` for "read this tag, clean it,
        // write it back") for the common single-`Extract`-with-`sanitize` case. Folded into
        // `topic_derivers` below (not kept as a separate list) so a category can override a
        // sanitizer's output exactly the way it overrides a deriver's — same mechanism, no new
        // JSON syntax needed.
        let sanitizer_fields: Vec<Field> = spec.sanitizers.iter().map(|s| s.to_field()).collect();

        // Load the data-defined sanitizer chains: shared (topics/_shared/sanitizers.json) merged
        // with the topic's own, topic-local winning on name conflict. Names win over built-ins.
        let read_sanitizers = |path: &std::path::Path| -> anyhow::Result<HashMap<String, SanitizerDef>> {
            if path.exists() {
                Ok(serde_json::from_str(&std::fs::read_to_string(path)?)
                    .with_context(|| format!("parsing {}", path.display()))?)
            } else {
                Ok(HashMap::new())
            }
        };
        let shared_dir = base.parent().expect("topics/<name> has a parent").join("_shared");
        let mut sanitizer_defs = read_sanitizers(&shared_dir.join("sanitizers.json"))?;
        for (k, v) in read_sanitizers(&base.join("sanitizers.json"))? {
            sanitizer_defs.insert(k, v); // topic-local overrides shared
        }
        let sanitizers = SanitizerRegistry::new(sanitizer_defs);

        // Load the deriver library (named single-output extractors). Optional: a topic with no
        // derivers (e.g. barrierLines) may omit the file.
        let derivers_path = base.join("derivers.json");
        let deriver_lib: HashMap<String, Producer> = if derivers_path.exists() {
            serde_json::from_str(&std::fs::read_to_string(&derivers_path)?)
                .with_context(|| format!("parsing topics/{name}/derivers.json"))?
        } else {
            HashMap::new()
        };

        // Resolve the topic-default deriver bindings (validates references), appended after the
        // sanitizer fields. A sanitizer and a topic-level deriver silently sharing an output would
        // mean the deriver wins with no visible signal *why* the sanitizer never took effect — so
        // that overlap is a config error, not a documented precedence rule. (A *category*
        // overriding a sanitizer's output is fine — that override is explicit by construction:
        // it names the output it's replacing.)
        let topic_derivers_resolved = resolve_bindings(&deriver_lib, &spec.derivers, name)?;
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

        // Merge shared cross-topic macros (topics/_shared/) into each kind's macro namespace.
        // Topic-local macros win on name conflict.
        let shared_dir = base.parent().expect("topics/<name> has a parent").join("_shared");
        let shared_macros = load_shared_macros(&shared_dir).with_context(|| "loading topics/_shared/")?;
        for cats in categories.values_mut() {
            for (k, v) in &shared_macros {
                cats.macros.entry(k.clone()).or_insert_with(|| v.clone());
            }
            // Compile the exclude relation into a priority order + discrimination tree (needs macros
            // fully merged first). categorize() is then pure first-match over this — no runtime excludes.
            cats.build_order(tree_max_depth)
                .with_context(|| format!("building category order for topics/{name}"))?;
        }

        // Split the declared transform pipeline into one ordered list of in-place `PreCatStep`s
        // (applied together, in declaration order, before `exclude_condition`/categorization) and
        // the parameterized center-line splits (which change object cardinality, handled
        // separately and always last).
        let mut pre_cat_steps = Vec::new();
        let mut transformations = Vec::new();
        for t in &spec.transforms {
            match t {
                ParamTransform::SplitSides {
                    highway, prefix, directed_keys, self_directed_keys,
                } => {
                    // `directed_keys`/`self_directed_keys` are just key names in JSON — translated
                    // here into ordinary `PreCatStep::TagRule`s (a `directed` `Producer::Extract`
                    // per key), applied per side object post-split (see `tag_engine::runner`). The
                    // split itself never sees these; it only unnests.
                    let directed_step = |key: &String, from: TagSet| PreCatStep::TagRule {
                        output: key.clone(),
                        source: Producer::Extract {
                            key: Some(key.clone()), keys: None, from, side: None, sanitize: None,
                            consts: Map::new(), directed: true,
                        },
                    };
                    let steps: Vec<PreCatStep> = directed_keys.iter().map(|k| directed_step(k, TagSet::Parent))
                        .chain(self_directed_keys.iter().map(|k| directed_step(k, TagSet::Obj)))
                        .collect();
                    transformations.push(CenterLineTransformation {
                        highway: Box::leak(highway.clone().into_boxed_str()),
                        prefix:  Box::leak(prefix.clone().into_boxed_str()),
                        directed_steps: Box::leak(steps.into_boxed_slice()),
                    });
                }
                ParamTransform::UnnestSidepathSelf { prefix } => {
                    let prefix = Box::leak(prefix.clone().into_boxed_str()) as &'static str;
                    pre_cat_steps.push(PreCatStep::SidepathSelf { prefix });
                }
                ParamTransform::TagRules { output, source } => {
                    pre_cat_steps.push(PreCatStep::TagRule {
                        output: output.clone(),
                        source: source.clone(),
                    });
                }
                ParamTransform::StripPrefix {
                    prefix, stamp_key, stamp_value, stamp_nested_under,
                } => {
                    pre_cat_steps.push(PreCatStep::StripPrefix {
                        prefix: prefix.clone(),
                        stamp_key: stamp_key.clone(),
                        stamp_value: stamp_value.clone(),
                        stamp_nested_under: stamp_nested_under.clone(),
                    });
                }
            }
        }
        let exclude_check_at = pre_cat_steps
            .iter()
            .position(|s| matches!(s, PreCatStep::SidepathSelf { .. }))
            .unwrap_or(pre_cat_steps.len());

        // Precompute per-category effective derivers/consts/private across every kind. Category ids
        // are expected unique within a topic (they're file stems); a node and a way category sharing
        // a stem would collide here — keep stems distinct per topic.
        let mut category_derivers = HashMap::new();
        let mut category_consts = HashMap::new();
        let mut category_private = HashMap::new();
        for cats in categories.values() {
            for cat in &cats.categories {
                if let Some(bindings) = &cat.derivers {
                    let overrides = resolve_bindings(&deriver_lib, bindings, name)?;
                    category_derivers
                        .insert(cat.id.clone(), apply_overrides(&topic_derivers, overrides));
                }
                category_consts.insert(cat.id.clone(), overlay(&spec.consts, &cat.consts));
                category_private.insert(cat.id.clone(), overlay(&spec.private, &cat.private));
            }
        }

        Ok(Self {
            spec,
            categories,
            pre_cat_steps,
            exclude_check_at,
            transformations,
            sanitizer_fields,
            sanitizers,
            deriver_lib,
            topic_derivers,
            category_derivers,
            category_consts,
            category_private,
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
    /// `build_topic_rows`, which applies `pre_cat_steps` (way kind only — they're way-oriented),
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
        let tags = crate::profile::time(&crate::profile::TAGCLONE, || raw_tags.clone());
        build_topic_rows(self, kind, osm_id, tags, meta)
    }
}
