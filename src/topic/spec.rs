//! The JSON schema types a topic's `topic.json` (plus, optionally, `transforms.json` — see
//! `TransformsSpec`) deserialize into, plus `resolve_output_entry`, which turns one raw `outputs`
//! map value into a resolved `Field`. Pure load-time data model — no per-object evaluation lives
//! here.

use std::collections::HashMap;

use anyhow::Context;
use serde::Deserialize;
use serde_json::{Map, Value};
use crate::lang::extract::Extract;
use crate::lang::filter::Filter;
use crate::lang::producer::Producer;
use crate::parser::TagSet;
use crate::lang::sanitize::{resolve_named_sanitizer, Sanitizer, StrOrVec};
use crate::categorize::transform::{CloneStep, DirectedFrom, InputTransform, TransformStep};

#[derive(Debug, Deserialize)]
pub struct TopicSpec {
    pub table: String,
    /// One entry per output field, keyed by output name — replaces the former separate
    /// `osm_fields`/`sanitizers`/`derivers` lists, all of which produced the same
    /// `Field{output, source: Producer}` shape and are now just different value shapes of one
    /// `outputs` map (see `resolve_output_entry`). A category can override any subset of these by
    /// declaring its own `outputs` map, merged over the topic's by key (category wins). Also
    /// accepts a bare `true`/`false` in place of the map (see `OutputsSpec`) — `true` means "pass
    /// every tag through verbatim", for scratch/inspection topics that don't want to enumerate
    /// fields.
    #[serde(default)]
    pub outputs: OutputsSpec,
    /// Optional Filter condition evaluated against raw way tags before categorization.
    /// If the condition matches, the way is skipped entirely for this topic.
    /// Uses the same Filter JSON syntax as category conditions.
    #[serde(default)]
    pub exclude_condition: Option<Filter>,
    /// Topic-level default values seeded into `produced` (lowest priority — any output producing
    /// the same key overrides them). Categories override per-key via their own `defaults`.
    #[serde(default)]
    pub defaults: serde_json::Map<String, serde_json::Value>,
    /// Which geometry outputs this topic wants, per element kind — replaces the old global
    /// `--emit-way-geometries`/`--emit-relation-geometries`/`--topic-edges` CLI flags with a
    /// per-topic declaration. See `GeometryShape`.
    #[serde(default)]
    pub geometry: GeometrySpec,
}

/// `topic.json`'s top-level `outputs`: normally a `{field: producer}` map, but a bare `true`/
/// `false` is also accepted — `false` behaves exactly like `{}` (no fields; `into_fields_map`
/// treats both the same way), while `true` means "pass every tag through verbatim" and is read
/// via `is_all` into `TopicRunner::pass_through_all_tags`, which bypasses per-field `Field`
/// evaluation entirely (see `pipeline::eval_fields`'s call site) rather than expanding to one
/// `Field` per possible tag.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum OutputsSpec {
    All(bool),
    Fields(Map<String, Value>),
}

impl Default for OutputsSpec {
    fn default() -> Self {
        OutputsSpec::Fields(Map::new())
    }
}

impl OutputsSpec {
    pub fn into_fields_map(self) -> Map<String, Value> {
        match self {
            OutputsSpec::All(_) => Map::new(),
            OutputsSpec::Fields(m) => m,
        }
    }

    pub fn is_all(&self) -> bool {
        matches!(self, OutputsSpec::All(true))
    }
}

/// Per-kind geometry output declarations (`topic.json`'s `"geometry"`). Every shape is now built
/// in-process (no Postgres post-import SQL step — see `geom`), so `node`/`way`/
/// `relation` all share the same `GeometryShape` vocabulary; `GeometrySpec::validate` rejects
/// combinations that don't make sense for a kind (e.g. a node can't have a `Line`).
#[derive(Debug, Deserialize, Default)]
pub struct GeometrySpec {
    /// Geometry outputs for this topic's nodes. Only `Point` is meaningful here.
    #[serde(default)]
    pub node: Vec<GeometryShape>,
    /// Geometry outputs for this topic's ways.
    #[serde(default)]
    pub way: Vec<GeometryShape>,
    /// Geometry outputs for this topic's relations — built from its member ways' already-resolved
    /// geometry (see `geom::relation::resolve_relation_ways`), no SQL post-processing needed.
    #[serde(default)]
    pub relation: Vec<GeometryShape>,
}

impl GeometrySpec {
    /// Reject shapes that are meaningless for their kind: `node` only ever supports `Point`
    /// (a bare point has no line/graph/polygon reading); `relation` never supports `Graph` (the
    /// routing/edge table is way-only — a relation has no natural directed-cost reading).
    pub fn validate(&self) -> anyhow::Result<()> {
        for shape in &self.node {
            anyhow::ensure!(
                *shape == GeometryShape::Point,
                "geometry.node only supports \"point\", got {shape:?}"
            );
        }
        for shape in &self.relation {
            anyhow::ensure!(
                *shape != GeometryShape::Graph,
                "geometry.relation does not support \"graph\" (routing/edge tables are way-only)"
            );
        }
        Ok(())
    }
}

/// One geometry output a topic can opt into for a given element kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GeometryShape {
    /// A single point: a node's own coordinate, or a way's/relation's centroid.
    Point,
    /// The whole (unsplit) linestring per kept way, or one merged multi-linestring per kept
    /// relation (its member ways' geometries collected + line-merged). `"linestring"` is accepted
    /// as a JSON alias for the old spelling.
    #[serde(alias = "linestring")]
    Line,
    /// Ways only: this topic's kept ways feed into a per-topic `{table}_edge` pgRouting-shaped
    /// table (intersection-split, `cost`/`reverse_cost` from the topic's own `cost`/`is_directed`
    /// fields — see `db::topic_edges`). Requires the topic to define a `cost` field.
    Graph,
    /// A closed ring (way) or assembled multipolygon (relation, from `outer`/`inner` member
    /// roles).
    Polygon,
}

/// One produced field: `{ output, source: Producer }`. The resolved form every `outputs` map
/// entry (see `resolve_output_entry`) turns into — used for the topic's own fields and every
/// category's effective fields alike, all sharing one eval path (`pipeline::eval_fields`).
#[derive(Debug, Clone)]
pub struct Field {
    pub output: String,
    pub source: Producer,
}

/// Resolve one raw `outputs` map value (topic- or category-level, already merged by key) into a
/// `Field`. Four value shapes, tried in this order:
/// - `true` — verbatim extract of the identically-named tag: `"surface": true` desugars to
///   `{ output: "surface", source: { key: "surface" } }`.
/// - a JSON string — a named reference into `producer_lib` (the topic's `producers.json`),
///   resolved once here rather than falling back silently on a miss (unlike
///   `resolve_named_sanitizer`'s builtin fallback) — a typo'd name should fail loudly at load time.
/// - an object shaped `{ name, in?, from? }` (no `key`/`keys`/`fallback`/`rules` — those
///   uniquely identify a full `Producer` instead): sugar for "read the first present of `in`
///   (default `[output]`) from `from` (default obj), clean it with the `name` sanitizer." The
///   map key supplies the output/default-input name, so unlike the old list-based sanitizer sugar
///   there's no redundant `tag` field. This is the one shape whose sanitizer name is never spelled
///   as a `sanitize:` field, so it's the one place here that still resolves a name directly
///   (`resolve_named_sanitizer`) rather than relying on `topic::load`'s JSON-level inlining.
/// - any other object — a full inline `Producer` (`Extract`/`Match`, or `fallback` sugar for a
///   `Match`; `Extract` already supports `sanitize` directly for the general case). Any macro/named
///   sanitizer reference inside is already resolved by the time `value` reaches here — it was
///   extracted from a raw `outputs` map that went through `topic::load::resolve_refs` before this
///   function's caller ever saw it — so `Producer::deserialize` needs no further resolution pass.
pub fn resolve_output_entry(
    output: &str,
    value: Value,
    producer_lib: &HashMap<String, Producer>,
    sanitizers: &HashMap<String, Sanitizer>,
) -> anyhow::Result<Producer> {
    let is_sanitizer_shorthand = matches!(&value, Value::Object(m) if m.contains_key("name")
        && !m.contains_key("key") && !m.contains_key("keys")
        && !m.contains_key("fallback") && !m.contains_key("rules"));

    let source = if is_sanitizer_shorthand {
        #[derive(Deserialize)]
        struct SanitizerRepr {
            name: String,
            #[serde(default, rename = "in")]
            in_keys: Option<StrOrVec>,
            #[serde(default)]
            from: TagSet,
        }
        let r: SanitizerRepr = serde_json::from_value(value)
            .with_context(|| format!("topic outputs.{output}"))?;
        let extract = Producer::Extract {
            extract: Extract::Candidates {
                keys: r.in_keys.map(StrOrVec::into_vec).unwrap_or_else(|| vec![output.to_owned()]),
                sanitize: Some(resolve_named_sanitizer(&r.name, sanitizers)),
            },
            annotate: Map::new(),
        };
        match r.from {
            TagSet::Obj => extract,
            TagSet::Parent => Producer::Parent(Box::new(extract)),
            TagSet::ParentOrObj => crate::parser::parent_or_obj(extract),
        }
    } else {
        match value {
            Value::Bool(true) => Producer::Extract {
                extract: Extract::Value { key: output.to_owned(), sanitize: None },
                annotate: Map::new(),
            },
            Value::Bool(false) => anyhow::bail!("topic outputs.{output}: `false` is not a valid entry"),
            Value::String(name) => producer_lib.get(&name).cloned().ok_or_else(|| {
                anyhow::anyhow!("topic outputs.{output}: producer '{name}' not found in producers.json")
            })?,
            other => Producer::deserialize(other).with_context(|| format!("topic outputs.{output}"))?,
        }
    };
    Ok(source)
}

// ── transforms.json ─────────────────────────────────────────────────────────────

/// A topic's whole transform pipeline, read from its own `transforms.json`, nested one
/// `KindTransformsSpec` per element kind (mirrors `GeometrySpec`'s `way`/`relation` split) — so a
/// topic can define separate pipelines for its nodes, ways, and relations instead of one
/// way-oriented pipeline for the whole topic. Optional per topic (see
/// `topic::load::load_topic_transforms`) — a topic with no `transforms.json` simply has an empty
/// pipeline for every kind (`TopicRunner::load` defaults to an empty `HashMap`).
#[derive(Debug, Deserialize, Default)]
pub struct TransformsSpec {
    #[serde(default)]
    pub node: KindTransformsSpec,
    #[serde(default)]
    pub way: KindTransformsSpec,
    #[serde(default)]
    pub relation: KindTransformsSpec,
}

/// One element kind's transform pipeline — a flat, ordered step list, always run in full *after*
/// `exclude_condition` is checked (see `topic::pipeline::build_topic_rows`). Earlier this split
/// into `before_exclude`/`after_exclude` phases around the exclude check, because some steps
/// (e.g. bikelanes'/roads' construction->real-highway rewrite) fed values `exclude_condition`
/// itself depended on. That dependency is now expressed directly in `exclude_condition` (see
/// `configs/tilda/macros.json`'s `is_allowed_highway`/`is_construction_highway` and
/// `configs/tilda/roads/macros.json`'s `effective_highway_eq_*`/`is_construction_or_blocked_raw`),
/// so `exclude_condition` only ever needs raw, untransformed tags — no phase split left to encode.
#[derive(Debug, Deserialize, Default)]
#[serde(transparent)]
pub struct KindTransformsSpec(pub Vec<PipelineStepSpec>);

impl KindTransformsSpec {
    /// Build the ready-to-run pipeline. Every step's `Filter`/`Producer` field is already resolved
    /// — the whole `transforms.json` document went through `topic::load::resolve_refs` before it
    /// was deserialized into `Self` — so this is pure reshaping into `InputTransform`/
    /// `TransformStep`, no macro/sanitizer lookup left to do.
    pub fn into_pipeline(self) -> Vec<TransformStep> {
        self.0.into_iter().map(PipelineStepSpec::into_transform_step).collect()
    }
}

impl TransformsSpec {
    /// Build the ready-to-run per-kind pipelines, keyed by `ElementKind` — only kinds with a
    /// non-empty pipeline get an entry, so `TopicRunner::pipelines.get(&kind)` naturally falls
    /// back to "no transforms" for the rest.
    pub fn into_pipelines(self) -> HashMap<crate::osm::types::ElementKind, Vec<TransformStep>> {
        use crate::osm::types::ElementKind;
        [(ElementKind::Node, self.node), (ElementKind::Way, self.way), (ElementKind::Relation, self.relation)]
            .into_iter()
            .filter(|(_, spec)| !spec.0.is_empty())
            .map(|(kind, spec)| (kind, spec.into_pipeline()))
            .collect()
    }
}

/// A leaf transform step — reused both for a top-level `before_exclude`/`after_exclude` phase and
/// for a `Clone`'s own `steps` (clones don't nest, so this is deliberately a narrower type than
/// `PipelineStepSpec`). Shape alone picks the variant, no `transform` discriminator to write.
#[derive(Debug)]
pub enum TransformSpec {
    /// `{ "output": ..., <producer fields> }`.
    TagRule { output: String, source: Producer },
    /// `{ "output": ..., "directed": { "key": ..., "from"?: "obj"|"parent", "sanitize"?: ... } }` —
    /// identified by its required `directed` field (checked before the generic `TagRule` catch-all,
    /// since a bare `Producer::deserialize` no longer accepts this shape). See
    /// `categorize::transform::InputTransform::DirectedExtract`.
    DirectedExtract {
        output: String,
        key: String,
        from: DirectedFrom,
        sanitize: Option<Sanitizer>,
    },
    /// `{ "prefix": ..., "stamp_key": ..., "stamp_value": ..., "stamp_nested_under"?: [...] }` —
    /// identified by its required `stamp_key` field.
    StripPrefix {
        prefix: String,
        stamp_key: String,
        stamp_value: String,
        stamp_nested_under: Vec<String>,
    },
    /// `{ "unnest": "<prefix>", "infix"?: "...", "meta"?: [...], "guard"?: <Filter>,
    /// "record_infix_as"?: "..." }` — identified by its required `unnest` field. See
    /// `categorize::transform::InputTransform::UnnestTags`.
    Unnest {
        prefix: String,
        infix: String,
        meta: Vec<String>,
        guard: Option<Filter>,
        record_infix_as: Option<String>,
    },
    /// `{ "drop": <Filter> }` — identified by its required `drop` field. See
    /// `categorize::transform::InputTransform::Drop`.
    Drop { when: Filter },
}

impl<'de> Deserialize<'de> for TransformSpec {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::Error;
        let v = Map::deserialize(deserializer)?;
        if v.contains_key("stamp_key") {
            #[derive(Deserialize)]
            struct Repr {
                prefix: String,
                stamp_key: String,
                stamp_value: String,
                #[serde(default)]
                stamp_nested_under: Vec<String>,
            }
            let r: Repr = serde_json::from_value(Value::Object(v)).map_err(D::Error::custom)?;
            Ok(TransformSpec::StripPrefix {
                prefix: r.prefix,
                stamp_key: r.stamp_key,
                stamp_value: r.stamp_value,
                stamp_nested_under: r.stamp_nested_under,
            })
        } else if v.contains_key("unnest") {
            #[derive(Deserialize)]
            struct Repr {
                unnest: String,
                #[serde(default)]
                infix: String,
                #[serde(default)]
                meta: Vec<String>,
                #[serde(default)]
                guard: Option<Filter>,
                #[serde(default)]
                record_infix_as: Option<String>,
            }
            let r: Repr = serde_json::from_value(Value::Object(v)).map_err(D::Error::custom)?;
            Ok(TransformSpec::Unnest { prefix: r.unnest, infix: r.infix, meta: r.meta, guard: r.guard, record_infix_as: r.record_infix_as })
        } else if v.contains_key("drop") {
            #[derive(Deserialize)]
            struct Repr { drop: Filter }
            let r: Repr = serde_json::from_value(Value::Object(v)).map_err(D::Error::custom)?;
            Ok(TransformSpec::Drop { when: r.drop })
        } else if v.contains_key("directed") {
            #[derive(Deserialize)]
            struct DirectedRepr {
                key: String,
                #[serde(default)]
                from: DirectedFrom,
                #[serde(default)]
                sanitize: Option<Sanitizer>,
            }
            #[derive(Deserialize)]
            struct Repr {
                output: String,
                directed: DirectedRepr,
            }
            let r: Repr = serde_json::from_value(Value::Object(v)).map_err(D::Error::custom)?;
            Ok(TransformSpec::DirectedExtract {
                output: r.output,
                key: r.directed.key,
                from: r.directed.from,
                sanitize: r.directed.sanitize,
            })
        } else {
            let output = v
                .get("output")
                .and_then(Value::as_str)
                .ok_or_else(|| D::Error::custom("transforms.json step needs `output`, `stamp_key`, `unnest`, or `drop`"))?
                .to_owned();
            let mut rest = v;
            rest.remove("output");
            let source = Producer::deserialize(Value::Object(rest)).map_err(D::Error::custom)?;
            Ok(TransformSpec::TagRule { output, source })
        }
    }
}

impl TransformSpec {
    fn into_input_transform(self) -> InputTransform {
        match self {
            TransformSpec::TagRule { output, source } => InputTransform::TagRule { output, source },
            TransformSpec::DirectedExtract { output, key, from, sanitize } =>
                InputTransform::DirectedExtract { output, key, from, sanitize },
            TransformSpec::StripPrefix { prefix, stamp_key, stamp_value, stamp_nested_under } =>
                InputTransform::StripPrefix { prefix, stamp_key, stamp_value, stamp_nested_under },
            TransformSpec::Unnest { prefix, infix, meta, guard, record_infix_as } => InputTransform::UnnestTags {
                prefix: Box::leak(prefix.into_boxed_str()),
                infix: Box::leak(infix.into_boxed_str()),
                meta_prefixes: Box::leak(
                    meta.into_iter().map(|s| Box::leak(s.into_boxed_str()) as &str).collect::<Vec<_>>().into_boxed_slice(),
                ),
                guard,
                record_infix_as: record_infix_as.map(|s| Box::leak(s.into_boxed_str()) as &str),
            },
            TransformSpec::Drop { when } => InputTransform::Drop { when },
        }
    }
}

/// A top-level pipeline step: any `TransformSpec` shape, or `{ "clone": { "when"?: <Filter>,
/// "annotate"?: {...}, "id_suffix": "...", "steps": [...] } }` — identified by being *only* that
/// one `clone` key. See `categorize::transform::CloneStep`.
#[derive(Debug)]
pub enum PipelineStepSpec {
    Transform(TransformSpec),
    Clone {
        when: Option<Filter>,
        annotate: Vec<(String, String)>,
        id_suffix: String,
        steps: Vec<TransformSpec>,
    },
}

impl<'de> Deserialize<'de> for PipelineStepSpec {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::Error;
        let v = Map::deserialize(deserializer)?;
        if v.len() == 1 && v.contains_key("clone") {
            #[derive(Deserialize)]
            struct Repr {
                #[serde(default)]
                when: Option<Filter>,
                #[serde(default)]
                annotate: std::collections::BTreeMap<String, String>,
                id_suffix: String,
                #[serde(default)]
                steps: Vec<TransformSpec>,
            }
            let r: Repr = serde_json::from_value(v["clone"].clone()).map_err(D::Error::custom)?;
            Ok(PipelineStepSpec::Clone {
                when: r.when,
                annotate: r.annotate.into_iter().collect(),
                id_suffix: r.id_suffix,
                steps: r.steps,
            })
        } else {
            Ok(PipelineStepSpec::Transform(TransformSpec::deserialize(Value::Object(v)).map_err(D::Error::custom)?))
        }
    }
}

impl PipelineStepSpec {
    fn into_transform_step(self) -> TransformStep {
        match self {
            PipelineStepSpec::Transform(t) => TransformStep::Transform(t.into_input_transform()),
            PipelineStepSpec::Clone { when, annotate, id_suffix, steps } => TransformStep::Clone(CloneStep {
                when,
                annotate,
                id_suffix,
                steps: steps.into_iter().map(TransformSpec::into_input_transform).collect(),
            }),
        }
    }
}

#[cfg(test)]
mod transforms_spec_tests {
    use super::*;
    use crate::osm::types::RawTags;

    fn tags<'a>(pairs: &[(&'a str, &'a str)]) -> RawTags<'a> {
        pairs.iter().map(|&(k, v)| (std::borrow::Cow::Borrowed(k), std::borrow::Cow::Borrowed(v))).collect()
    }

    /// A cycleway left/right split, authored the way a topic's own `transforms.json` would,
    /// parsed end to end (JSON string -> `TransformsSpec` -> resolved `Vec<TransformStep>`) and
    /// run through the real engine — the one thing the hand-written `Deserialize` impls above
    /// have no other test coverage for, since no topic has a `transforms.json` file yet.
    #[test]
    fn cycleway_split_parses_and_runs() {
        let json = r#"
        {
          "way": [
            {
              "clone": {
                "when": { "not": { "tag": "highway", "eq": "cycleway" } },
                "annotate": { "_side": "left", "_prefix": "cycleway" },
                "id_suffix": "cycleway/left",
                "steps": [
                  { "unnest": "cycleway", "infix": "", "record_infix_as": "_infix" },
                  { "unnest": "cycleway", "infix": "both", "record_infix_as": "_infix" },
                  { "unnest": "cycleway", "infix": "left", "record_infix_as": "_infix" },
                  { "drop": { "tags_empty": true } },
                  { "output": "highway", "rules": [], "default": "cycleway" }
                ]
              }
            }
          ]
        }
        "#;
        let spec: TransformsSpec = serde_json::from_str(json).expect("parse transforms.json");
        assert_eq!(spec.way.0.len(), 1);

        let pipeline = spec.way.into_pipeline();
        assert_eq!(pipeline.len(), 1);

        let mut way_tags = tags(&[("highway", "primary"), ("cycleway:left", "lane")]);
        let mut annotations = Map::new();
        let mut clones = Vec::new();
        let kept = crate::categorize::transform::run_transform_steps(&mut way_tags, &mut annotations, &pipeline, "way/1", &mut clones);
        assert!(kept);
        assert_eq!(clones.len(), 1);
        let (clone_tags, clone_annotations, id) = &clones[0];
        assert_eq!(id, "way/1/cycleway/left");
        assert_eq!(clone_tags.get("cycleway").map(|v| v.as_ref()), Some("lane"));
        assert_eq!(clone_tags.get("highway").map(|v| v.as_ref()), Some("cycleway"));
        assert_eq!(clone_annotations.get("_infix").and_then(Value::as_str), Some("left"));
    }

    #[test]
    fn drop_shape_parses() {
        let step: TransformSpec = serde_json::from_value(serde_json::json!({ "drop": { "tags_empty": true } })).unwrap();
        assert!(matches!(step, TransformSpec::Drop { .. }));
    }

    #[test]
    fn tag_rule_shape_still_parses_alongside_new_shapes() {
        let step: TransformSpec = serde_json::from_value(serde_json::json!({ "output": "surface", "key": "surface" })).unwrap();
        assert!(matches!(step, TransformSpec::TagRule { .. }));
    }
}
