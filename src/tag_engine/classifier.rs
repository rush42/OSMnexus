//! A generic, data-driven classifier: an ordered list of `{ when, value }` rules, evaluated
//! against a way's tags with the shared `Filter` engine. The first rule whose condition matches
//! *and* produces something yields the value — `value` is an arbitrary `Producer`, which subsumes
//! a literal (`Producer::Const`) as well as any nested read/branch; rules are first-match-wins,
//! with an optional `default`.
//!
//! Named, cross-topic shared rule tables (e.g. the `road` classifier) are a *config-loading*
//! concept, not one this module or `Producer` knows about: `topic::load::inline_shared_producers`
//! substitutes a `{ "shared": "<name>" }` reference with the named table's own JSON before any of
//! it is deserialized, at topic-directory-read time — the same treatment shared macros/sanitizers
//! get. By the time a `Rule` reaches this module, it's indistinguishable from one that was always
//! topic-local.

use std::collections::HashMap;

use serde::Deserialize;
use serde_json::{Map, Value};

use crate::tag_engine::filter::{eval, Filter, FilterSpec};
use crate::tag_engine::producer::{ExtractCtx, Produced, Producer, ProducerSpec};
use crate::tag_engine::sanitize::Sanitizer;

/// One classifier rule, **as parsed** — `when`/`value` are the as-parsed `*Spec` tiers (may carry
/// macros/named sanitizers). Resolved into `Rule` by `resolve`.
#[derive(Debug, Clone, Deserialize)]
pub struct RuleSpec {
    pub when: FilterSpec,
    pub value: ProducerSpec,
}

impl RuleSpec {
    /// Resolve this rule's `when` (macro/sanitizer expansion) and `value` (nested producer
    /// resolution) into a runtime `Rule`.
    pub fn resolve(
        &self,
        macros: &HashMap<String, FilterSpec>,
        sanitizers: &HashMap<String, Sanitizer>,
    ) -> anyhow::Result<Rule> {
        Ok(Rule {
            when: self.when.expand(macros, sanitizers)?,
            value: self.value.resolve(macros, sanitizers)?,
        })
    }
}

/// One classifier rule, **resolved** — what `match_rules` evaluates.
#[derive(Debug, Clone)]
pub struct Rule {
    pub when: Filter,
    pub value: Producer,
}

/// First rule whose `when` holds *and* whose value actually produces something, in order — a
/// matching rule that produces nothing doesn't stop the search, it just tries the next rule. This
/// is what lets `Producer::Match` subsume a plain ordered fallback chain (every rule `when: true`,
/// first branch that produces anything wins) as well as a conditional (one real `when`, then an
/// unconditional trailing rule for the "else"), in addition to the classic flat classify table.
///
/// A winning branch that carries its own `annotate` (e.g. an `Extract`'s own field, explicitly
/// set) is used as-is; a branch that produces an *empty* `annotate` (e.g. a bare `Producer::Const`
/// literal, which has no way to spell one) inherits `own_consts` — the enclosing `Producer::Match`'s
/// own `annotate` field — instead.
///
/// Shared by the standalone `road` classifier and the data-defined `rules` value producer
/// (`tag_engine::producer`). Evaluated against a full `ExtractCtx` — same predicate evaluator
/// (`filter::eval`) and same context shape category matching uses, so a rule's `when` can see
/// side/prefix/infix/parent, not just raw tags. Does not apply a `default` — callers needing one
/// (e.g. `Producer::Match`) apply it themselves.
pub fn match_rules(rules: &[Rule], ctx: &ExtractCtx, own_consts: &Map<String, Value>) -> Option<Produced> {
    for rule in rules {
        if !eval(&rule.when, ctx) {
            continue;
        }
        if let Some(mut produced) = rule.value.eval(ctx) {
            if produced.annotate.is_empty() {
                produced.annotate = own_consts.clone();
            }
            return Some(produced);
        }
    }
    None
}
