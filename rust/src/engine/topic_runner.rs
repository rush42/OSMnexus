use std::collections::HashMap;

use anyhow::Context;
use bytes::Bytes;
use futures::SinkExt;

use crate::classify::categories::{load_categories_from_dir, load_shared_macros, CategoriesFile};
use crate::classify::sanitize::{SanitizerDef, SanitizerRegistry};
use crate::engine::extract::Producer;
use crate::engine::{runner::{build_topic_rows, TopicRow}, topic::{DeriverBinding, Field, Transform, TopicSpec}};
use crate::osm::types::{OsmWay, RawTags};
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
    pub categories: CategoriesFile,
    /// No-arg tag transforms applied (in order) to each way's tags before categorization.
    pub tag_transforms: Vec<TagTransform>,
    /// Center-line side split (from a `split_sides` entry); empty if the topic has none.
    pub transformations: Vec<CenterLineTransformation>,
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
    /// Load a topic from its directory under `topics/<name>/`.
    pub fn load(name: &str) -> anyhow::Result<Self> {
        let base = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("topics/{name}"));

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

        let cats_dir = base.join("categories");
        let mut categories = if cats_dir.exists() {
            load_categories_from_dir(&cats_dir)
                .with_context(|| format!("loading topics/{name}/categories/"))?
        } else {
            // No categories/ dir → nothing matches, so the topic emits no rows.
            // Every shipped topic has a categories/ dir; this is just a safe fallback.
            CategoriesFile { macros: Default::default(), categories: Vec::new() }
        };

        // Merge shared cross-topic macros (topics/_shared/) into this topic's macro
        // namespace. Topic-local macros win on name conflict.
        let shared_dir = base.parent().expect("topics/<name> has a parent").join("_shared");
        for (k, v) in load_shared_macros(&shared_dir)
            .with_context(|| "loading topics/_shared/")?
        {
            categories.macros.entry(k).or_insert(v);
        }

        // Split the declared transform pipeline into ordered no-arg tag transforms and
        // the (at most one) parameterized center-line split.
        let mut tag_transforms = Vec::new();
        let mut transformations = Vec::new();
        for t in &spec.transforms {
            match t {
                Transform::Named(tname) => tag_transforms.push(
                    TagTransform::from_name(tname)
                        .with_context(|| format!("topics/{name}/topic.json transforms"))?,
                ),
                Transform::SplitSides { transform, highway, prefix } => {
                    anyhow::ensure!(
                        transform == "split_sides",
                        "unknown parameterized transform '{transform}' in topics/{name}/topic.json",
                    );
                    transformations.push(CenterLineTransformation {
                        highway: Box::leak(highway.clone().into_boxed_str()),
                        prefix:  Box::leak(prefix.clone().into_boxed_str()),
                    });
                }
            }
        }

        // Precompute effective derivers for categories that override one (validates references).
        let mut category_derivers = HashMap::new();
        for cat in &categories.categories {
            if let Some(bindings) = &cat.derivers {
                let overrides = resolve_bindings(&deriver_lib, bindings, name)?;
                category_derivers.insert(cat.id.clone(), apply_overrides(&topic_derivers, overrides));
            }
        }

        // Precompute effective consts/private per category: topic-level defaults overlaid per-key.
        let mut category_consts = HashMap::new();
        let mut category_private = HashMap::new();
        for cat in &categories.categories {
            category_consts.insert(cat.id.clone(), overlay(&spec.consts, &cat.consts));
            category_private.insert(cat.id.clone(), overlay(&spec.private, &cat.private));
        }

        Ok(Self {
            spec,
            categories,
            tag_transforms,
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

    /// Run the topic's pipeline for one way: apply this topic's tag transforms to a copy
    /// of the raw tags, then categorize/split/extract. `raw_tags` are the way's untouched
    /// tags — each topic transforms its own copy.
    pub fn process(&self, way: &OsmWay, raw_tags: &RawTags, geom: &geo::LineString<f64>, length_m: f64, meta: &OsmMeta) -> Vec<TopicRow> {
        let mut tags = raw_tags.clone();
        for t in &self.tag_transforms {
            t.apply(&mut tags);
        }
        build_topic_rows(self, way, &tags, geom, length_m, meta)
    }
}

const FLUSH_BYTES: usize = 512 * 1024;

/// Write a slice of TopicRows into any COPY sink, flushing every 512 KB.
pub async fn stream_rows<S>(
    rows: Vec<TopicRow>,
    buf: &mut Vec<u8>,
    mut sink: std::pin::Pin<&mut S>,
) -> anyhow::Result<usize>
where
    S: futures::Sink<Bytes, Error = tokio_postgres::Error>,
{
    let mut n = 0;
    for row in rows {
        let fields = row.to_csv_fields()?;
        write_csv_row(buf, &fields);
        n += 1;
        if buf.len() >= FLUSH_BYTES {
            sink.as_mut().send(Bytes::from(std::mem::take(buf))).await?;
            *buf = Vec::with_capacity(FLUSH_BYTES);
        }
    }
    Ok(n)
}

fn write_csv_row(buf: &mut Vec<u8>, fields: &[String]) {
    for (i, field) in fields.iter().enumerate() {
        if i > 0 { buf.push(b','); }
        let needs_quoting = field.contains('"') || field.contains(',')
            || field.contains('\n') || field.contains('\\');
        if needs_quoting {
            buf.push(b'"');
            for ch in field.chars() {
                if ch == '"' { buf.extend_from_slice(b"\"\""); }
                else {
                    let mut tmp = [0u8; 4];
                    buf.extend_from_slice(ch.encode_utf8(&mut tmp).as_bytes());
                }
            }
            buf.push(b'"');
        } else {
            buf.extend_from_slice(field.as_bytes());
        }
    }
    buf.push(b'\n');
}
