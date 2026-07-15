//! A generic, data-driven classifier: an ordered list of `{ when, value }` rules, evaluated
//! against a way's tags with the shared `Filter` engine. The first rule whose condition matches
//! *and* produces something yields the value (any JSON literal, or an arbitrary nested `Producer`
//! — see `ValueSpec`); rules are first-match-wins, with an optional `default`.
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

use crate::tag_engine::filter::{eval, Filter};
use crate::tag_engine::producer::{ExtractCtx, Produced, Producer};
use crate::tag_engine::sanitize::Sanitizer;

/// The value a matching rule produces. `Const` holds any JSON literal (string, number, bool) so
/// the same rule table can back string classifiers (category ids, `road`), numeric ones (zoom),
/// and boolean ones (subsuming what used to be separate `FilterZoom`/`FilterMatch` producers).
/// `Producer` lets a rule's output be an arbitrary nested producer (e.g. an `Extract` with its own
/// `keys`/`sanitize`/`from`) — this is what lets `rules` subsume `cond`'s `then`/`else`: a
/// condition becomes a rule whose `when` is that condition and whose `value` is the `then`
/// producer, followed by an unconditional (`"when": true`) rule holding the `else` producer. The
/// old `{"tag": ...}`/`{"tag_or": ..., "or": ...}` shorthands are JSON-only sugar now, folded into
/// `Producer` (a plain `Extract`, or a `Match` using its own `default`) by `tag_engine::parser`'s
/// hand-written `Deserialize` — see that module's doc for why. `Deserialize` isn't derived here for
/// the same reason it isn't on `Producer` itself: a stray derive could reintroduce a shape `parser`
/// doesn't know about. `Producer` must be tried before `Const` there, since `Const(Value)` is an
/// untagged catch-all that would otherwise consume any object literal (including a producer's own
/// JSON shape) first.
#[derive(Debug, Clone)]
pub enum ValueSpec {
    Producer(Box<Producer>),
    /// A literal value.
    Const(Value),
}

impl ValueSpec {
    /// Resolve any nested `Producer`'s macros/sanitizers once at load time (`Const` carries no
    /// named references, so it passes through unchanged).
    pub fn resolve(
        &self,
        macros: &HashMap<String, Filter>,
        sanitizers: &HashMap<String, Sanitizer>,
    ) -> anyhow::Result<ValueSpec> {
        Ok(match self {
            ValueSpec::Producer(p) => ValueSpec::Producer(Box::new(p.resolve(macros, sanitizers)?)),
            ValueSpec::Const(v) => ValueSpec::Const(v.clone()),
        })
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Rule {
    pub when: Filter,
    pub value: ValueSpec,
}

/// First rule whose `when` holds *and* whose value actually produces something, in order — a
/// matching rule that produces nothing doesn't stop the search, it just tries the next rule. This
/// is what lets `Producer::Match` subsume a plain ordered fallback chain (every rule `when: true`,
/// first branch that produces anything wins) as well as a conditional (one real `when`, then an
/// unconditional trailing rule for the "else"), in addition to the classic flat classify table.
///
/// A `ValueSpec::Producer` branch carries its own provenance (`Produced::annotate`) — e.g. an
/// `Extract`'s own `annotate` field — which is used as-is. A literal branch (`Tag`/`TagOr`/`Const`)
/// carries none of its own, so `own_consts` (the enclosing `Producer::Match`'s `annotate` field)
/// supplies its provenance instead.
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
        let produced = match &rule.value {
            ValueSpec::Const(v) => Some(Produced { value: v.clone(), annotate: own_consts.clone() }),
            ValueSpec::Producer(p) => p.eval(ctx),
        };
        if produced.is_some() {
            return produced;
        }
    }
    None
}
