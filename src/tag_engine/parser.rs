//! JSON-only sugar for the tag_engine's public types, kept in one place — cleanly separated from
//! their runtime definitions (`Producer` in `producer.rs`; `Sanitizer`/`Step` in `sanitize.rs`).
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

use crate::tag_engine::classifier::{Rule, ValueSpec};
use crate::tag_engine::filter::Filter;
use crate::tag_engine::producer::{Producer, TagSet};
use crate::tag_engine::sanitize::{ReplaceRule, SanitizeRef, Sanitizer, Step, StrOrVec};

// ── Producer ─────────────────────────────────────────────────────────────────

/// The JSON shapes `Producer` accepts: `Match`/`Extract` verbatim, plus `Fallback`'s `fallback`
/// sugar — folded into an equivalent `Match` in `Deserialize` below, so a `Producer` value is
/// never observably a fallback chain, only ever `Match`/`Extract`. Untagged, tried in this order
/// (more-specific/required-field shapes before `Extract`, whose fields are all optional and so
/// would otherwise match everything first).
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum ProducerJson {
    /// Try each branch in order; the first one that produces anything wins, carrying its own
    /// branch-level `consts`. Desugars to an all-`when: true` `Match` (see `classifier::match_rules`
    /// for why a matching-but-empty rule doesn't stop the search — that's what makes this
    /// equivalence exact).
    Fallback { fallback: Vec<Producer> },
    Match {
        rules: Vec<Rule>,
        #[serde(default)] default: Option<Value>,
        #[serde(default)] from: TagSet,
        #[serde(default)] consts: Map<String, Value>,
    },
    Extract {
        #[serde(default)] key: Option<String>,
        #[serde(default)] keys: Option<Vec<String>>,
        #[serde(default)] from: TagSet,
        #[serde(default)] side: Option<String>,
        #[serde(default)] sanitize: Option<SanitizeRef>,
        #[serde(default)] consts: Map<String, Value>,
        #[serde(default)] directed: bool,
    },
}

impl<'de> Deserialize<'de> for Producer {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Ok(match ProducerJson::deserialize(deserializer)? {
            ProducerJson::Fallback { fallback } => Producer::Match {
                rules: fallback.into_iter()
                    .map(|p| Rule { when: Filter::Bool(true), value: ValueSpec::Producer(Box::new(p)) })
                    .collect(),
                default: None,
                from: TagSet::Obj,
                consts: Map::new(),
            },
            ProducerJson::Match { rules, default, from, consts } => Producer::Match { rules, default, from, consts },
            ProducerJson::Extract { key, keys, from, side, sanitize, consts, directed } =>
                Producer::Extract { key, keys, from, side, sanitize, consts, directed },
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
