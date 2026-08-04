//! JSON-only sugar for the `lang`/`categorize` engines' public types, kept in one place — cleanly
//! separated from their runtime definitions (`Producer` in `lang/producer.rs`; `Sanitizer`/`Step`
//! in `lang/sanitize.rs`).
//! Each hand-written `Deserialize` impl here folds an extra on-disk JSON shape (`fallback`; a bare
//! single sanitize step instead of an array; `cases`/`filter`/`drop`) into the type's own canonical
//! form, backed by a private, `#[serde(untagged)]` parse-only helper enum that mirrors the old
//! JSON shapes (`ProducerJson`/`SanitizerJson`/`StepJson`). None of these helpers is ever
//! observable outside this module — the public type's own value never carries the pre-folded
//! shape, not even transiently. This is a recurring recipe in this codebase (see the
//! `[[json-sugar-collapse-pattern]]` memory): whenever two JSON shapes are really the same runtime
//! behavior spelled differently, collapse them here rather than growing another enum variant the
//! runtime type (and its `eval`/`resolve`) would have to keep handling.
//!
//! A *named, cross-file* reference (e.g. a shared classifier table, `{ "shared": "<name>" }`) is
//! deliberately NOT handled here — that's not JSON sugar this module folds, it's a `topic::load`
//! concern (`inline_shared_producers`, run once at topic-directory-read time, before any of this
//! is even reached) — see `producer.rs`'s own doc for why.

use std::collections::HashMap;
use std::ops::Bound;

use serde::{Deserialize, Deserializer};
use serde_json::{Map, Value};

use crate::lang::extract::Extract;
use crate::lang::filter::Filter;
use crate::lang::producer::{Producer, Rule};
use crate::lang::sanitize::{AtomicJson, Builtin, ReplaceRule, Sanitizer, StrOrVec};

// ── Producer ─────────────────────────────────────────────────────────────────

/// Which tagset a bare `{name, from}` sanitizer-shorthand output entry reads (`topic::spec`) — every
/// tagset-scoping need here goes through `Producer::Parent`/`parent_or_obj` wrapping the base
/// producer (see their docs), never a field carried at runtime; `TagSet` is JSON vocabulary that
/// picks the wrapper, not a `Producer` field itself.
#[derive(Debug, Deserialize, Clone, Copy, Default)]
#[serde(rename_all = "snake_case")]
pub enum TagSet {
    #[default]
    Obj,
    /// Strict parent way: nothing if the object has no parent (matches old osm `parent`).
    Parent,
    /// Parent way, falling back to the object's own tags when there is no parent
    /// (matches the old yes_flag `source: parent`). Commits to the parent tagset when a
    /// parent exists — distinct from a `fallback:[{parent},{obj}]`, which would also fall
    /// through when the parent merely lacks the key.
    ParentOrObj,
}

/// The `ParentOrObj` equivalent for `p` — see `Producer::Parent`'s doc for why this is built here
/// rather than existing as its own variant. Used by `ProducerJson::ParentOrObj`'s `parent_or_obj`
/// JSON sugar, `topic::spec`'s sanitizer-shorthand `from: parent_or_obj`, and Rust-side synthesis
/// (`topic::runner`) that composes already-resolved producers — all three build/compose plain
/// `Producer` values now, no separate as-parsed tier. Its two rules are tried in priority order
/// like any other `Match`.
pub fn parent_or_obj(p: Producer) -> Producer {
    Producer::Match {
        rules: vec![
            Rule { when: Filter::HasParent { has_parent: true }, value: Producer::Parent(Box::new(p.clone())) },
            Rule { when: Filter::HasParent { has_parent: false }, value: p },
        ],
        default: None,
        annotate: Map::new(),
        tree: None,
    }
}

/// The JSON shapes `Producer` accepts: `Match`/`Extract` verbatim, `Parent` wrapping any nested
/// `Producer` shape to scope it to the parent way's tags, `Fallback`'s `fallback` and
/// `ParentOrObj`'s `parent_or_obj` sugar (both folded into an equivalent `Match` in `Deserialize`
/// below, so a `Producer` value is never observably either), and
/// `Tag`'s `{"tag": ..., "or"?: ...}` shorthand (folds into a plain `Extract`, or — when `or` is
/// present — a `Match` using its own `default` for the "or" branch; neither ever exists as its own
/// runtime variant); a bare literal needs no sugar here at all, since `Const`'s a real variant that
/// deserializes straight from a bare JSON value. Untagged, tried in this order
/// (more-specific/required-field shapes before `Extract`, whose fields are all optional and so
/// would otherwise match everything first, and before `Const`, the bare-JSON catch-all).
///
/// A direction-sensitive read (`{ "directed": {...} }`) is deliberately NOT a `Producer` shape —
/// see `categorize::transform::InputTransform::DirectedExtract`'s own doc for why it moved to its
/// own transform-pipeline step instead.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum ProducerJson {
    /// Scope the wrapped producer to the parent way's tags — see `Producer::Parent`.
    Parent { parent: Box<Producer> },
    /// Scope to the parent's tags, falling back to the object's own when there's no parent — see
    /// `parent_or_obj` for the `Match`+`Parent` equivalence this desugars to.
    ParentOrObj { parent_or_obj: Box<Producer> },
    /// Try each branch in order; the first one that produces anything wins, carrying its own
    /// branch-level `annotate`. Desugars to an all-`when: true` `Match` (see `producer::match_rules`
    /// for why a matching-but-empty rule doesn't stop the search — that's what makes this
    /// equivalence exact).
    Fallback { fallback: Vec<Producer> },
    /// Copy a tag's own value (e.g. fall back to the raw `highway` value), or — when `or` is
    /// present — that literal instead if the tag is absent.
    Tag {
        tag: String,
        #[serde(default)]
        or: Option<Value>,
    },
    Match {
        rules: Vec<Rule>,
        #[serde(default)] default: Option<Value>,
        #[serde(default)] annotate: Map<String, Value>,
    },
    Extract {
        #[serde(default)] key: Option<String>,
        #[serde(default)] keys: Option<Vec<String>>,
        #[serde(default, deserialize_with = "deserialize_sanitize_chain")] sanitize: Vec<Sanitizer>,
        #[serde(default)] annotate: Map<String, Value>,
    },
    /// A literal value, independent of any tag.
    Const(Value),
}

impl<'de> Deserialize<'de> for Producer {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Ok(match ProducerJson::deserialize(deserializer)? {
            ProducerJson::Parent { parent } => Producer::Parent(parent),
            ProducerJson::ParentOrObj { parent_or_obj: inner } => parent_or_obj(*inner),
            ProducerJson::Fallback { fallback } => Producer::Match {
                rules: fallback.into_iter().map(|value| Rule { when: Filter::Bool(true), value }).collect(),
                default: None,
                annotate: Map::new(),
                tree: None,
            },
            ProducerJson::Tag { tag, or: None } => {
                Producer::Extract { extract: Extract::Value { key: tag, sanitize: Vec::new() }, annotate: Map::new() }
            }
            ProducerJson::Tag { tag, or: Some(or) } => Producer::Match {
                rules: vec![Rule {
                    when: Filter::Bool(true),
                    value: Producer::Extract { extract: Extract::Value { key: tag, sanitize: Vec::new() }, annotate: Map::new() },
                }],
                default: Some(or),
                annotate: Map::new(),
                tree: None,
            },
            ProducerJson::Match { rules, default, annotate } => {
                Producer::Match { rules, default, annotate, tree: None }
            }
            ProducerJson::Extract { key, keys, sanitize, annotate } => {
                let extract = match (key, keys) {
                    (Some(key), None) => Extract::Value { key, sanitize },
                    (None, Some(keys)) => Extract::Candidates { keys, sanitize },
                    (None, None) => return Err(serde::de::Error::custom("Extract needs `key` or `keys`")),
                    (Some(_), Some(_)) => return Err(serde::de::Error::custom("Extract: set only one of `key`/`keys`, not both")),
                };
                Producer::Extract { extract, annotate }
            }
            ProducerJson::Const(value) => Producer::Const { value, annotate: Map::new() },
        })
    }
}

// ── Sanitizer chain ──────────────────────────────────────────────────────────

/// The JSON shapes a `sanitize:` field accepts: a single step (any of `Sanitizer`'s own shapes,
/// including the bare-string `Builtin` alias), or an explicit array of steps. Untagged, array
/// tried first (a step is never itself a JSON array). The `deserialize_with` behind every
/// `sanitize: Vec<Sanitizer>` field (`Extract`'s two variants, `ProducerJson::Extract`,
/// `topic::spec`'s `directed` sugar) and, via `parse_sanitize_chain`, every `sanitizers.json` entry
/// (`topic::load::load_topic_sanitizers`) — `Vec<Sanitizer>` is a foreign type here, so this can't
/// be a `Deserialize` impl on it directly (orphan rule); a `deserialize_with` function is the
/// idiomatic escape hatch.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum SanitizeChainJson {
    Chain(Vec<Sanitizer>),
    One(Sanitizer),
}

pub(crate) fn deserialize_sanitize_chain<'de, D>(deserializer: D) -> Result<Vec<Sanitizer>, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(match SanitizeChainJson::deserialize(deserializer)? {
        SanitizeChainJson::Chain(steps) => steps,
        SanitizeChainJson::One(step) => vec![step],
    })
}

/// Same folding as `deserialize_sanitize_chain`, over an already-parsed `Value` rather than a live
/// `Deserializer` — what `topic::load::load_topic_sanitizers` needs, since a `sanitizers.json`
/// entry is read out of a `HashMap<String, Value>` rather than deserialized field-by-field.
pub(crate) fn parse_sanitize_chain(value: Value) -> Result<Vec<Sanitizer>, serde_json::Error> {
    Ok(match serde_json::from_value(value)? {
        SanitizeChainJson::Chain(steps) => steps,
        SanitizeChainJson::One(step) => vec![step],
    })
}

// ── Sanitizer ────────────────────────────────────────────────────────────────

/// The JSON shapes `Sanitizer` accepts — see `Sanitizer`'s own doc for how `Cases`/`Filter`/`Drop`
/// fold into `Mapping`. Untagged; object shapes are distinguished by their required field name, so
/// order among them doesn't matter, but the bare-string `Builtin` must be tried where a plain JSON
/// string naturally falls out (it's the only variant a string can deserialize into).
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum StepJson {
    Mapping {
        mapping: HashMap<String, Value>,
        #[serde(default)]
        on_miss: Option<String>,
    },
    Cases {
        cases: HashMap<String, StrOrVec>,
        #[serde(default)]
        on_miss: Option<String>,
    },
    Filter { filter: Vec<String> },
    Drop { drop: Vec<String> },
    Replace { replace: Vec<ReplaceRule> },
    Builtin(String),
}

/// Convert one JSON-side mapping value to its canonical `Sanitizer::Mapping` entry: `Value::Null`
/// is the "found, but drop anyway" sentinel (`None`); any other atomic (string/bool/number) becomes
/// `Some`; a nested array/object is rejected — `Sanitizer::Mapping` entries are never anything else
/// (see `Sanitizer`'s own doc).
fn mapping_entry<E: serde::de::Error>(v: Value) -> Result<Option<AtomicJson>, E> {
    match &v {
        Value::Null => Ok(None),
        _ => AtomicJson::from_value(&v)
            .map(Some)
            .ok_or_else(|| E::custom(format!("mapping entry must be a string/bool/number or null, got {v}"))),
    }
}

impl<'de> Deserialize<'de> for Sanitizer {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Ok(match StepJson::deserialize(deserializer)? {
            StepJson::Mapping { mapping, on_miss } => Sanitizer::Mapping {
                mapping: mapping.into_iter().map(|(k, v)| Ok((k, mapping_entry(v)?))).collect::<Result<_, D::Error>>()?,
                on_miss,
            },
            StepJson::Cases { cases, on_miss } => Sanitizer::Mapping {
                mapping: cases.into_iter()
                    .flat_map(|(output, inputs)| {
                        inputs.into_vec().into_iter().map(move |input| (input, Some(AtomicJson::Str(output.clone()))))
                    })
                    .collect(),
                on_miss,
            },
            StepJson::Filter { filter } => Sanitizer::Mapping {
                mapping: filter.into_iter().map(|v| (v.clone(), Some(AtomicJson::Str(v)))).collect(),
                on_miss: None,
            },
            StepJson::Drop { drop } => Sanitizer::Mapping {
                mapping: drop.into_iter().map(|v| (v, None)).collect(),
                on_miss: Some("keep".to_owned()),
            },
            StepJson::Replace { replace } => Sanitizer::Replace { replace },
            StepJson::Builtin(name) => Sanitizer::Builtin(
                Builtin::from_name(&name).ok_or_else(|| serde::de::Error::custom(format!("unknown built-in sanitizer: {name}")))?,
            ),
        })
    }
}

// ── Filter ───────────────────────────────────────────────────────────────────

/// The JSON shapes `Filter` accepts: every real variant verbatim, including `Filter::AnnotationEq`
/// spelled directly as `{"annotation": <key>, "eq": <value>}` — see `Filter::AnnotationEq`'s own
/// doc. Untagged, tried in this order (more-specific/required-field shapes before `Eq`, whose
/// `#[serde(flatten)] extract` field is optional-shaped enough to otherwise match too broadly).
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum FilterJson {
    Bool(bool),
    And { and: Vec<Filter> },
    Or { or: Vec<Filter> },
    Not { not: Box<Filter> },
    InSet { #[serde(flatten)] extract: Extract, in_set: String },
    In { #[serde(flatten)] extract: Extract, r#in: Vec<String> },
    Contains { #[serde(flatten)] extract: Extract, contains: String, #[serde(default)] case_insensitive: bool },
    StartsWith { #[serde(flatten)] extract: Extract, starts_with: String },
    EndsWith { #[serde(flatten)] extract: Extract, ends_with: String },
    Exists { #[serde(flatten)] extract: Extract, exists: bool },
    Parent { parent: Box<Filter> },
    Annotation { annotation: String, eq: String },
    HasKeyPrefix { has_key_prefix: String },
    HasParent { has_parent: bool },
    TagsEmpty { tags_empty: bool },
    NumLt { #[serde(flatten)] extract: Extract, lt: f64 },
    NumLte { #[serde(flatten)] extract: Extract, lte: f64 },
    NumGt { #[serde(flatten)] extract: Extract, gt: f64 },
    NumGte { #[serde(flatten)] extract: Extract, gte: f64 },
    Eq { #[serde(flatten)] extract: Extract, eq: String },
}

impl<'de> Deserialize<'de> for Filter {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Ok(match FilterJson::deserialize(deserializer)? {
            FilterJson::Bool(b) => Filter::Bool(b),
            FilterJson::And { and } => Filter::And { and },
            FilterJson::Or { or } => Filter::Or { or },
            FilterJson::Not { not } => Filter::Not { not },
            FilterJson::InSet { extract, in_set } => Filter::InSet { extract, in_set },
            FilterJson::In { extract, r#in } => Filter::In { extract, r#in },
            FilterJson::Contains { extract, contains, case_insensitive } =>
                Filter::Contains { extract, contains, case_insensitive },
            FilterJson::StartsWith { extract, starts_with } => Filter::StartsWith { extract, starts_with },
            FilterJson::EndsWith { extract, ends_with } => Filter::EndsWith { extract, ends_with },
            FilterJson::Exists { extract, exists } => Filter::Exists { extract, exists },
            FilterJson::Eq { extract, eq } => Filter::Eq { extract, eq },
            FilterJson::Parent { parent } => Filter::Parent { parent },
            FilterJson::Annotation { annotation, eq } => Filter::AnnotationEq { key: annotation, eq },
            FilterJson::HasKeyPrefix { has_key_prefix } => Filter::HasKeyPrefix { has_key_prefix },
            FilterJson::HasParent { has_parent } => Filter::HasParent { has_parent },
            FilterJson::TagsEmpty { tags_empty } => Filter::TagsEmpty { tags_empty },
            FilterJson::NumLt { extract, lt } =>
                Filter::NumRange { extract, min: Bound::Unbounded, max: Bound::Excluded(lt) },
            FilterJson::NumLte { extract, lte } =>
                Filter::NumRange { extract, min: Bound::Unbounded, max: Bound::Included(lte) },
            FilterJson::NumGt { extract, gt } =>
                Filter::NumRange { extract, min: Bound::Excluded(gt), max: Bound::Unbounded },
            FilterJson::NumGte { extract, gte } =>
                Filter::NumRange { extract, min: Bound::Included(gte), max: Bound::Unbounded },
        })
    }
}
