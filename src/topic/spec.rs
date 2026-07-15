//! The JSON schema types a topic's `topic.json` (plus, optionally, `transforms.json` — see
//! `TransformsSpec`) deserialize into, plus `resolve_output_entry`, which turns one raw `outputs`
//! map value into a resolved `Field`. Pure load-time data model — no per-object evaluation lives
//! here.

use std::collections::HashMap;

use anyhow::Context;
use serde::Deserialize;
use serde_json::{Map, Value};
use crate::tag_engine::extract::Extract;
use crate::tag_engine::filter::Filter;
use crate::tag_engine::producer::{Producer, TagSet};
use crate::tag_engine::sanitize::{SanitizeRef, Sanitizer, StrOrVec};
use crate::tag_engine::transform::{CloneStep, InputTransform, TransformStep};

#[derive(Debug, Deserialize)]
pub struct TopicSpec {
    pub table: String,
    /// Center-line splits — the engine's one built-in cardinality-changing transform (unnests a
    /// side's tags onto its own object; see `SplitSidesSpec`). Always applied last, after every
    /// `input_transforms` entry, regardless of where it'd fall in declaration order — so it lives
    /// in its own top-level list rather than interleaved into `input_transforms`.
    #[serde(default)]
    pub split_sides: Vec<SplitSidesSpec>,
    /// Ordered pipeline of in-place tag rewrites, applied before categorization (see
    /// `tag_engine::transform::InputTransform`). Each entry is data-driven — no `transform`
    /// discriminator: a bare `{ "output": ..., <producer fields> }` (the common case) writes
    /// `output` from any full `Producer`; `strip_prefix`/`unnest_sidepath_self`-shaped entries (see
    /// `InputTransformSpec`) are the few operations needing dynamic key iteration a single
    /// `Producer` output can't express.
    #[serde(default)]
    pub input_transforms: Vec<InputTransformSpec>,
    /// One entry per output field, keyed by output name — replaces the former separate
    /// `osm_fields`/`sanitizers`/`derivers` lists, all of which produced the same
    /// `Field{output, source: Producer}` shape and are now just different value shapes of one
    /// `outputs` map (see `resolve_output_entry`). A category can override any subset of these by
    /// declaring its own `outputs` map, merged over the topic's by key (category wins).
    #[serde(default)]
    pub outputs: Map<String, Value>,
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

/// Per-kind geometry output declarations (`topic.json`'s `"geometry"`). Node geometry (raw point)
/// isn't wired up yet — only `way`/`relation` shapes exist today.
#[derive(Debug, Deserialize, Default)]
pub struct GeometrySpec {
    /// Geometry outputs for this topic's ways.
    #[serde(default)]
    pub way: Vec<GeometryShape>,
    /// Geometry outputs for this topic's relations. Always a post-processing SQL step (Postgres
    /// output only) — a relation is classified before any member way's geometry is resolved (see
    /// `db::topic_geometries`), unlike `way`'s shapes, which are computed during streaming.
    #[serde(default)]
    pub relation: Vec<GeometryShape>,
}

/// One geometry output a topic can opt into for a given element kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GeometryShape {
    /// Ways only: this topic's kept ways feed into a per-topic `{table}_edge` pgRouting-shaped
    /// table (intersection-split, `cost`/`reverse_cost` from the topic's own `cost`/`is_directed`
    /// fields — see `db::topic_edges`). Requires the topic to define a `cost` field.
    Graph,
    /// The whole (unsplit) linestring per kept way, or one merged multi-linestring per kept
    /// relation (its member ways' geometries collected + line-merged) — see
    /// `db::topic_geometries`.
    Linestring,
}

/// One center-line split: unnest tags with `prefix` onto a side object whose effective highway
/// becomes `highway`. List the entry once per projection, e.g.
/// `{ "highway": "cycleway", "prefix": "cycleway", "directed_keys": ["cycleway:lanes", "bicycle:lanes"] }`.
/// `directed_keys` lists parent-way tags that are direction-sensitive (have `:forward`/`:backward`
/// variants); each is projected onto the side object, preferring the directed variant matching
/// that side, read from the *parent* way's tags. `self_directed_keys` is the same idea but read
/// from the side object's *own* tags instead — for tags that arrive on the object already suffixed
/// (e.g. a `cycleway:both:traffic_sign:forward` tag unnests to the object's own
/// `traffic_sign:forward` key, not the parent's).
#[derive(Debug, Deserialize)]
pub struct SplitSidesSpec {
    pub highway: String,
    pub prefix: String,
    #[serde(default)]
    pub directed_keys: Vec<String>,
    #[serde(default)]
    pub self_directed_keys: Vec<String>,
}

/// An `input_transforms` entry. Shape alone picks the variant (see `Deserialize` below) — no
/// `transform` discriminator to write.
#[derive(Debug)]
pub enum InputTransformSpec {
    /// Strip `prefix` from matching keys, re-key onto the base tag, and stamp a lifecycle-style
    /// marker (`<base>:<stamp_key>` when nested under one of `stamp_nested_under`, else `stamp_key`).
    /// The one step needing dynamic key iteration, so it isn't expressible as a bare `Producer`.
    /// Identified by its required `stamp_key` field.
    StripPrefix {
        prefix: String,
        stamp_key: String,
        stamp_value: String,
        stamp_nested_under: Vec<String>,
    },
    /// For ways whose own `highway` is a sidepath class (see the `sidepath_highway` value set),
    /// unnest bare `prefix`-prefixed tags (and their `source:`/`note:` meta variants) onto the way
    /// itself, plus derive `traffic_sign` from `traffic_sign:forward` for oneway cycleways. Models
    /// the OSM convention of tagging a way's own cycling function directly on it (e.g.
    /// `highway=path` + `cycleway=track`), as opposed to `split_sides` projecting side tags onto
    /// separate child objects. Also needs dynamic key iteration. Identified by being *only*
    /// `{ "prefix": ... }` — nothing else has that exact single-key shape.
    UnnestSidepathSelf { prefix: String },
    /// Write `output` from any full `Producer` (`rules`/`fallback`/`key`/`cond` — the same shape
    /// used for `outputs`), evaluated against the way's own tags (no parent, no category/side
    /// context yet) and applied as a raw-tag mutation *before* categorization — so, unlike an
    /// `outputs` entry, this can influence which category a way matches.
    /// A produced `null` deletes `output`; a produced non-null value must be a string and
    /// overwrites it; no match (`None`) leaves `output` untouched. The default (and by far most
    /// common) shape — identified by its required `output` field. e.g. deriving `traffic_sign`
    /// from `traffic_sign:forward` for oneway sidepaths:
    /// `{ "output": "traffic_sign", "rules": [
    ///      { "when": { "and": [ { "tag": "highway", "in_set": "sidepath_highway" },
    ///          { "tag": "traffic_sign", "exists": false }, { "tag": "oneway", "eq": "yes" },
    ///          { "not": { "tag": "oneway:bicycle", "eq": "no" } } ] },
    ///        "value": { "tag_or": "traffic_sign:forward", "or": "" } } ] }`.
    TagRules {
        output: String,
        source: Producer,
    },
}

impl<'de> serde::Deserialize<'de> for InputTransformSpec {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::Error;
        let v = serde_json::Map::deserialize(deserializer)?;
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
            Ok(InputTransformSpec::StripPrefix {
                prefix: r.prefix,
                stamp_key: r.stamp_key,
                stamp_value: r.stamp_value,
                stamp_nested_under: r.stamp_nested_under,
            })
        } else if v.len() == 1 && v.contains_key("prefix") {
            let prefix = v["prefix"].as_str().ok_or_else(|| D::Error::custom("`prefix` must be a string"))?;
            Ok(InputTransformSpec::UnnestSidepathSelf { prefix: prefix.to_owned() })
        } else {
            let output = v
                .get("output")
                .and_then(Value::as_str)
                .ok_or_else(|| D::Error::custom("input_transforms entry needs an `output` field"))?
                .to_owned();
            let mut rest = v;
            rest.remove("output");
            let source = Producer::deserialize(Value::Object(rest)).map_err(D::Error::custom)?;
            Ok(InputTransformSpec::TagRules { output, source })
        }
    }
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
///   resolved once here rather than falling back silently on a miss (unlike `SanitizeRef`'s
///   builtin fallback) — a typo'd name should fail loudly at load time.
/// - an object shaped `{ name, in?, from? }` (no `key`/`keys`/`fallback`/`rules` — those
///   uniquely identify a full `Producer` instead): sugar for "read the first present of `in`
///   (default `[output]`) from `from` (default obj), clean it with the `name` sanitizer." The
///   map key supplies the output/default-input name, so unlike the old list-based sanitizer sugar
///   there's no redundant `tag` field.
/// - any other object — a full inline `Producer` (`Extract`/`Match`, or `fallback` sugar for a
///   `Match`; `Extract` already supports `sanitize` directly for the general case).
pub fn resolve_output_entry(
    output: &str,
    value: Value,
    producer_lib: &HashMap<String, Producer>,
) -> anyhow::Result<Field> {
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
            },
            sanitize: Some(SanitizeRef::Name(r.name)),
            consts: Map::new(),
        };
        match r.from {
            TagSet::Obj => extract,
            TagSet::Parent => Producer::Parent(Box::new(extract)),
            TagSet::ParentOrObj => Producer::parent_or_obj(extract),
            TagSet::Annotations => anyhow::bail!(
                "topic outputs.{output}: `from: annotations` is only supported for directed reads, not the sanitizer shorthand"
            ),
        }
    } else {
        match value {
            Value::Bool(true) => Producer::Extract {
                extract: Extract::Value { key: output.to_owned() },
                sanitize: None,
                consts: Map::new(),
            },
            Value::Bool(false) => anyhow::bail!("topic outputs.{output}: `false` is not a valid entry"),
            Value::String(name) => producer_lib.get(&name).cloned().ok_or_else(|| {
                anyhow::anyhow!("topic outputs.{output}: producer '{name}' not found in producers.json")
            })?,
            other => Producer::deserialize(other).with_context(|| format!("topic outputs.{output}"))?,
        }
    };
    Ok(Field { output: output.to_owned(), source })
}

// ── transforms.json ─────────────────────────────────────────────────────────────

/// A topic's whole transform pipeline, read from its own `transforms.json` — explicit about where
/// `exclude_condition` is evaluated (`before_exclude` runs first, then `exclude_condition`, then
/// `after_exclude`) rather than inferring a cut point from step shapes the way the legacy
/// `input_transforms`/`split_sides` topic.json keys do. Optional per topic (see
/// `topic::load::load_topic_transforms`) — when absent, `TopicRunner::load` falls back to
/// synthesizing an equivalent pipeline from those legacy keys instead.
#[derive(Debug, Deserialize, Default)]
pub struct TransformsSpec {
    #[serde(default)]
    pub before_exclude: Vec<PipelineStepSpec>,
    #[serde(default)]
    pub after_exclude: Vec<PipelineStepSpec>,
}

impl TransformsSpec {
    /// Resolve every step's macro/sanitizer references and return the ready-to-run pipeline plus
    /// `exclude_check_at` (always `before_exclude.len()`, by construction).
    pub fn resolve(
        &self,
        macros: &HashMap<String, Filter>,
        sanitizers: &HashMap<String, Sanitizer>,
    ) -> anyhow::Result<(Vec<TransformStep>, usize)> {
        let mut pipeline: Vec<TransformStep> = self.before_exclude.iter()
            .map(|s| s.resolve(macros, sanitizers))
            .collect::<anyhow::Result<_>>()?;
        let exclude_check_at = pipeline.len();
        pipeline.extend(
            self.after_exclude.iter()
                .map(|s| s.resolve(macros, sanitizers))
                .collect::<anyhow::Result<Vec<_>>>()?,
        );
        Ok((pipeline, exclude_check_at))
    }
}

/// A leaf transform step — reused both for a top-level `before_exclude`/`after_exclude` phase and
/// for a `Clone`'s own `steps` (clones don't nest, so this is deliberately a narrower type than
/// `PipelineStepSpec`). Shape alone picks the variant, no `transform` discriminator — same
/// convention `InputTransformSpec` uses.
#[derive(Debug)]
pub enum TransformSpec {
    /// `{ "output": ..., <producer fields> }` — same shape as `InputTransformSpec::TagRules`.
    TagRule { output: String, source: Producer },
    /// `{ "prefix": ..., "stamp_key": ..., "stamp_value": ..., "stamp_nested_under"?: [...] }` —
    /// identified by its required `stamp_key` field, same as `InputTransformSpec::StripPrefix`.
    StripPrefix {
        prefix: String,
        stamp_key: String,
        stamp_value: String,
        stamp_nested_under: Vec<String>,
    },
    /// `{ "unnest": "<prefix>", "infix"?: "...", "meta"?: [...], "guard"?: <Filter>,
    /// "record_infix_as"?: "..." }` — identified by its required `unnest` field. See
    /// `tag_engine::transform::InputTransform::UnnestTags`.
    Unnest {
        prefix: String,
        infix: String,
        meta: Vec<String>,
        guard: Option<Filter>,
        record_infix_as: Option<String>,
    },
    /// `{ "drop": <Filter> }` — identified by its required `drop` field. See
    /// `tag_engine::transform::InputTransform::Drop`.
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
    fn resolve(
        &self,
        macros: &HashMap<String, Filter>,
        sanitizers: &HashMap<String, Sanitizer>,
    ) -> anyhow::Result<InputTransform> {
        Ok(match self {
            TransformSpec::TagRule { output, source } => InputTransform::TagRule {
                output: output.clone(),
                source: source.resolve(macros, sanitizers)?,
            },
            TransformSpec::StripPrefix { prefix, stamp_key, stamp_value, stamp_nested_under } => InputTransform::StripPrefix {
                prefix: prefix.clone(),
                stamp_key: stamp_key.clone(),
                stamp_value: stamp_value.clone(),
                stamp_nested_under: stamp_nested_under.clone(),
            },
            TransformSpec::Unnest { prefix, infix, meta, guard, record_infix_as } => InputTransform::UnnestTags {
                prefix: Box::leak(prefix.clone().into_boxed_str()),
                infix: Box::leak(infix.clone().into_boxed_str()),
                meta_prefixes: Box::leak(
                    meta.iter().map(|s| Box::leak(s.clone().into_boxed_str()) as &str).collect::<Vec<_>>().into_boxed_slice(),
                ),
                guard: guard.as_ref().map(|f| f.expand(macros, sanitizers)).transpose()?,
                record_infix_as: record_infix_as.as_ref().map(|s| Box::leak(s.clone().into_boxed_str()) as &str),
            },
            TransformSpec::Drop { when } => InputTransform::Drop { when: when.expand(macros, sanitizers)? },
        })
    }
}

/// A top-level pipeline step: any `TransformSpec` shape, or `{ "clone": { "when"?: <Filter>,
/// "annotate"?: {...}, "id_suffix": "...", "steps": [...] } }` — identified by being *only* that
/// one `clone` key. See `tag_engine::transform::CloneStep`.
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
    fn resolve(
        &self,
        macros: &HashMap<String, Filter>,
        sanitizers: &HashMap<String, Sanitizer>,
    ) -> anyhow::Result<TransformStep> {
        Ok(match self {
            PipelineStepSpec::Transform(t) => TransformStep::Transform(t.resolve(macros, sanitizers)?),
            PipelineStepSpec::Clone { when, annotate, id_suffix, steps } => TransformStep::Clone(CloneStep {
                when: when.as_ref().map(|f| f.expand(macros, sanitizers)).transpose()?,
                annotate: annotate.clone(),
                id_suffix: id_suffix.clone(),
                steps: steps.iter().map(|s| s.resolve(macros, sanitizers)).collect::<anyhow::Result<_>>()?,
            }),
        })
    }
}

#[cfg(test)]
mod transforms_spec_tests {
    use super::*;
    use crate::osm::types::RawTags;

    fn tags(pairs: &[(&str, &str)]) -> RawTags {
        pairs.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect()
    }

    /// A cycleway left/right split, authored the way a topic's own `transforms.json` would,
    /// parsed end to end (JSON string -> `TransformsSpec` -> resolved `Vec<TransformStep>`) and
    /// run through the real engine — the one thing the hand-written `Deserialize` impls above
    /// have no other test coverage for, since no topic has a `transforms.json` file yet.
    #[test]
    fn cycleway_split_parses_and_runs() {
        let json = r#"
        {
          "after_exclude": [
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
        assert_eq!(spec.after_exclude.len(), 1);

        let macros = HashMap::new();
        let sanitizers = HashMap::new();
        let (pipeline, exclude_check_at) = spec.resolve(&macros, &sanitizers).expect("resolve transforms.json");
        assert_eq!(exclude_check_at, 0);
        assert_eq!(pipeline.len(), 1);

        let mut way_tags = tags(&[("highway", "primary"), ("cycleway:left", "lane")]);
        let mut annotations = Map::new();
        let mut clones = Vec::new();
        let kept = crate::tag_engine::transform::run_transform_steps(&mut way_tags, &mut annotations, &pipeline, "way/1", &mut clones);
        assert!(kept);
        assert_eq!(clones.len(), 1);
        let (clone_tags, clone_annotations, id) = &clones[0];
        assert_eq!(id, "way/1/cycleway/left");
        assert_eq!(clone_tags.get("cycleway").map(String::as_str), Some("lane"));
        assert_eq!(clone_tags.get("highway").map(String::as_str), Some("cycleway"));
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
