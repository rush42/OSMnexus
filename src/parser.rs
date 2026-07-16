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

use serde::{Deserialize, Deserializer};
use serde_json::{Map, Value};

use crate::lang::extract::{DirectedFrom, DirectedKey, Extract};
use crate::lang::filter::Filter;
use crate::lang::producer::{MatchOrigin, Producer, Rule};
use crate::lang::sanitize::{ReplaceRule, Sanitizer, Step, StrOrVec};

// ── Producer ─────────────────────────────────────────────────────────────────

/// The JSON shapes `Producer` accepts: `Match`/`Extract` verbatim, `Parent` wrapping any nested
/// `Producer` shape to scope it to the parent way's tags, `Fallback`'s `fallback` and
/// `ParentOrObj`'s `parent_or_obj` sugar (both folded into an equivalent `Match` in `Deserialize`
/// below, so a `Producer` value is never observably either), `Directed`'s `directed` sugar for
/// `Producer::DirectedExtract` (`{ "directed": { "key": ..., "from"?: "obj"|"parent"|"annotations" } }`
/// — `from` is `DirectedFrom`, not the general `TagSet` (no `parent_or_obj`; see `DirectedFrom`'s own
/// doc for why), defaulting to `obj` same as `TagSet` does), and
/// `Tag`/`TagOr`'s `{"tag": ...}`/`{"tag_or": ..., "or": ...}` shorthands (fold into a plain
/// `Extract`, or a `Match` using its own `default` for the "or" branch — so neither ever exists as
/// its own runtime variant; a bare literal needs no sugar here at all, since `Const`'s a real
/// variant that deserializes straight from a bare JSON value). Untagged, tried in this order
/// (more-specific/required-field shapes before `Extract`, whose fields are all optional and so
/// would otherwise match everything first, and before `Const`, the bare-JSON catch-all).
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum ProducerJson {
    /// Scope the wrapped producer to the parent way's tags — see `Producer::Parent`.
    Parent { parent: Box<Producer> },
    /// Scope to the parent's tags, falling back to the object's own when there's no parent — see
    /// `Producer::parent_or_obj` for the `Match`+`Parent` equivalence this desugars to.
    ParentOrObj { parent_or_obj: Box<Producer> },
    /// Try each branch in order; the first one that produces anything wins, carrying its own
    /// branch-level `annotate`. Desugars to an all-`when: true` `Match` (see `producer::match_rules`
    /// for why a matching-but-empty rule doesn't stop the search — that's what makes this
    /// equivalence exact).
    Fallback { fallback: Vec<Producer> },
    /// Direction-sensitive read of `key` — see `Extract::Directed`.
    Directed { directed: DirectedRepr },
    /// Copy a tag's own value (e.g. fall back to the raw `highway` value).
    Tag { tag: String },
    /// Copy `tag_or`'s value, or the literal `or` when the tag is absent.
    TagOr { tag_or: String, or: Value },
    Match {
        rules: Vec<Rule>,
        #[serde(default)] default: Option<Value>,
        #[serde(default)] annotate: Map<String, Value>,
    },
    Extract {
        #[serde(default)] key: Option<String>,
        #[serde(default)] keys: Option<Vec<String>>,
        #[serde(default)] sanitize: Option<Sanitizer>,
        #[serde(default)] annotate: Map<String, Value>,
    },
    /// A literal value, independent of any tag.
    Const(Value),
}

#[derive(Debug, Clone, Deserialize)]
struct DirectedRepr {
    key: String,
    #[serde(default)]
    from: DirectedFrom,
    #[serde(default)]
    sanitize: Option<Sanitizer>,
    #[serde(default)]
    annotate: Map<String, Value>,
}

impl<'de> Deserialize<'de> for Producer {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Ok(match ProducerJson::deserialize(deserializer)? {
            ProducerJson::Parent { parent } => Producer::Parent(parent),
            ProducerJson::ParentOrObj { parent_or_obj } => Producer::parent_or_obj(*parent_or_obj),
            ProducerJson::Fallback { fallback } => Producer::Match {
                rules: fallback.into_iter().map(|value| Rule { when: Filter::Bool(true), value }).collect(),
                default: None,
                annotate: Map::new(),
                origin: MatchOrigin::Fallback,
            },
            ProducerJson::Directed { directed } => Producer::Extract {
                extract: Extract::Directed {
                    directed: DirectedKey { key: directed.key, from: directed.from },
                    sanitize: directed.sanitize,
                },
                annotate: directed.annotate,
            },
            ProducerJson::Tag { tag } => {
                Producer::Extract { extract: Extract::Value { key: tag, sanitize: None }, annotate: Map::new() }
            }
            ProducerJson::TagOr { tag_or, or } => Producer::Match {
                rules: vec![Rule {
                    when: Filter::Bool(true),
                    value: Producer::Extract { extract: Extract::Value { key: tag_or, sanitize: None }, annotate: Map::new() },
                }],
                default: Some(or),
                annotate: Map::new(),
                origin: MatchOrigin::TagOr,
            },
            ProducerJson::Match { rules, default, annotate } => {
                Producer::Match { rules, default, annotate, origin: MatchOrigin::Rules }
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

// ── Sanitizer ────────────────────────────────────────────────────────────────

/// The JSON shapes a `Sanitizer` accepts: a single step (any of `Step`'s own shapes, including the
/// bare-string `Builtin` alias), or an explicit array of steps. Untagged, array tried first (a
/// step is never itself a JSON array).
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum SanitizerJson {
    Chain(Vec<Step>),
    One(Step),
}

impl<'de> Deserialize<'de> for Sanitizer {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Ok(match SanitizerJson::deserialize(deserializer)? {
            SanitizerJson::Chain(steps) => Sanitizer::from_steps(steps),
            SanitizerJson::One(step) => Sanitizer::from_steps(vec![step]),
        })
    }
}

// ── Step ─────────────────────────────────────────────────────────────────────

/// The JSON shapes `Step` accepts — see `Step`'s own doc for how `Cases`/`Filter`/`Drop` fold into
/// `Mapping`. Untagged; object shapes are distinguished by their required field name, so order
/// among them doesn't matter, but the bare-string `Builtin` must be tried where a plain JSON
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

impl<'de> Deserialize<'de> for Step {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Ok(match StepJson::deserialize(deserializer)? {
            StepJson::Mapping { mapping, on_miss } => Step::Mapping { mapping, on_miss },
            StepJson::Cases { cases, on_miss } => Step::Mapping {
                mapping: cases.into_iter()
                    .flat_map(|(output, inputs)| {
                        inputs.into_vec().into_iter().map(move |input| (input, Value::String(output.clone())))
                    })
                    .collect(),
                on_miss,
            },
            StepJson::Filter { filter } => Step::Mapping {
                mapping: filter.into_iter().map(|v| (v.clone(), Value::String(v))).collect(),
                on_miss: None,
            },
            StepJson::Drop { drop } => Step::Mapping {
                mapping: drop.into_iter().map(|v| (v, Value::Null)).collect(),
                on_miss: Some("keep".to_owned()),
            },
            StepJson::Replace { replace } => Step::Replace { replace },
            StepJson::Builtin(name) => Step::Builtin(name),
        })
    }
}
