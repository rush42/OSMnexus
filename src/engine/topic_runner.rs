use std::collections::HashMap;

use anyhow::Context;

use crate::classify::categories::{load_shared_macros, load_topic_categories, CategoriesFile};
use crate::classify::sanitize::{SanitizerDef, SanitizerRegistry};
use crate::engine::extract::Producer;
use crate::engine::{runner::build_topic_rows, topic::{DeriverBinding, Field, ParamTransform, Transform, TopicSpec}};
use crate::osm::types::{ElementKind, RawTags};
use crate::output::rows::TopicRow;
use crate::output::types::OsmMeta;
use crate::transform::TagTransform;
use crate::transform::side_split::CenterLineTransformation;

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

/// A fully loaded topic ready to process ways.
pub struct TopicRunner {
    pub spec: TopicSpec,
    /// Category sets keyed by element kind — one per `topics/<t>/{node,way,relation}/` subfolder
    /// that exists. Each pass (relations → ways → nodes) classifies with its kind's set; a topic
    /// with only a `way/` folder has just the `Way` entry. `categorize` is the same function for all.
    pub categories: HashMap<ElementKind, CategoriesFile>,
    /// No-arg tag transforms applied (in order) to each way's tags before categorization.
    pub tag_transforms: Vec<TagTransform>,
    /// Center-line side split (from a `split_sides` entry); empty if the topic has none.
    pub transformations: Vec<CenterLineTransformation>,
    /// Prefixes to self-unnest onto sidepath-class ways (from `unnest_sidepath_self` entries).
    /// Applied after `exclude_condition` but before `transformations`, mirroring the pre-split
    /// stage `split_sides` also runs at — see `side_split::apply_sidepath_self`.
    pub sidepath_self_prefixes: Vec<&'static str>,
    /// `tag_rules` entries: `(output, rules)`, applied at the same pre-categorization stage as
    /// `sidepath_self_prefixes` (see `classify::classifier::classify_rules`).
    pub tag_rules: Vec<(String, Vec<crate::classify::classifier::Rule>)>,
    /// Desugared `sanitizers` — applied to every object regardless of category.
    pub sanitizer_fields: Vec<Field>,
    /// Data-defined sanitizer chains (sanitizers.json) layered over the built-in registry.
    pub sanitizers: SanitizerRegistry,
    /// The deriver library (derivers.json) — kept so Rust derivers can re-evaluate a sibling
    /// by name (e.g. smoothness_parent re-runs the base smoothness fallback on the parent).
    pub deriver_lib: HashMap<String, Producer>,
    /// Topic-default derivers (resolved from `derivers.json` via `topic.json`'s bindings).
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

        // Sanitizers desugar to per-object `Field`s, applied to every category.
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

        // Resolve the topic-default deriver bindings (validates references).
        let topic_derivers = resolve_bindings(&deriver_lib, &spec.derivers, name)?;

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

        // Split the declared transform pipeline into ordered in-place tag transforms and the
        // parameterized center-line splits (which change object cardinality, handled separately).
        let mut tag_transforms = Vec::new();
        let mut transformations = Vec::new();
        let mut sidepath_self_prefixes = Vec::new();
        let mut tag_rules = Vec::new();
        for t in &spec.transforms {
            match t {
                Transform::Named(tname) => {
                    anyhow::ensure!(
                        tname == "lifecycle",
                        "unknown named transform '{tname}' in topics/{name}/topic.json",
                    );
                    tag_transforms.push(TagTransform::Lifecycle);
                }
                Transform::Param(ParamTransform::SplitSides {
                    highway, prefix, directed_keys, self_directed_keys,
                }) => {
                    let leak_keys = |keys: &[String]| -> &'static [&'static str] {
                        let leaked: Vec<&'static str> = keys
                            .iter()
                            .map(|k| -> &'static str { Box::leak(k.clone().into_boxed_str()) })
                            .collect();
                        Box::leak(leaked.into_boxed_slice())
                    };
                    transformations.push(CenterLineTransformation {
                        highway: Box::leak(highway.clone().into_boxed_str()),
                        prefix:  Box::leak(prefix.clone().into_boxed_str()),
                        directed_keys: leak_keys(directed_keys),
                        self_directed_keys: leak_keys(self_directed_keys),
                    });
                }
                Transform::Param(ParamTransform::UnnestSidepathSelf { prefix }) => {
                    sidepath_self_prefixes.push(Box::leak(prefix.clone().into_boxed_str()) as &'static str);
                }
                Transform::Param(ParamTransform::TagRules { output, rules }) => {
                    tag_rules.push((output.clone(), rules.clone()));
                }
                Transform::Param(ParamTransform::RenameKey { from, to, when_value }) => {
                    tag_transforms.push(TagTransform::RenameKey {
                        from: from.clone(),
                        to: to.clone(),
                        when_value: when_value.clone(),
                    });
                }
                Transform::Param(ParamTransform::ValueCases { tag, remove_tag, cases }) => {
                    tag_transforms.push(TagTransform::ValueCases {
                        tag: tag.clone(),
                        remove_tag: *remove_tag,
                        cases: cases.clone(),
                    });
                }
                Transform::Param(ParamTransform::StripPrefix {
                    prefix, stamp_key, stamp_value, stamp_nested_under,
                }) => {
                    tag_transforms.push(TagTransform::StripPrefix {
                        prefix: prefix.clone(),
                        stamp_key: stamp_key.clone(),
                        stamp_value: stamp_value.clone(),
                        stamp_nested_under: stamp_nested_under.clone(),
                    });
                }
            }
        }

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
            tag_transforms,
            transformations,
            sidepath_self_prefixes,
            tag_rules,
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

    /// Run the topic's pipeline for one element of `kind`: apply this topic's tag transforms to a
    /// copy of the raw tags (way kind only — side-split/lifecycle transforms are way-oriented),
    /// then categorize/split/extract into tag rows against the kind's category set. `raw_tags` are
    /// the element's untouched tags. Geometry is produced separately (way-only) via `build_geom_rows`.
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
        let mut tags = raw_tags.clone();
        if kind == ElementKind::Way {
            for t in &self.tag_transforms {
                t.apply(&mut tags);
            }
        }
        build_topic_rows(self, kind, osm_id, tags, meta)
    }
}
