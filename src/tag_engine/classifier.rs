//! A generic, data-driven classifier: an ordered list of `{ when, value }` rules, evaluated
//! against a way's tags with the shared `Filter` engine. The first rule whose condition matches
//! yields the value (any JSON literal); rules are first-match-wins, with an optional `default`.
//!
//! The value is either a literal (string/number/bool) or a `{ "tag": "<key>" }` passthrough that
//! copies the tag's own value (used e.g. to fall back to the raw `highway` value). All domain
//! knowledge lives in the JSON; this module is just the evaluator, plus the load-time registry of
//! shared, named classifiers (`<config_root>/producers.json`, `{name: Classifier}`) a
//! `Producer::SharedClassify` resolves against.

use std::collections::HashMap;
use std::sync::OnceLock;

use serde::Deserialize;
use serde_json::Value;

use crate::tag_engine::filter::{eval, Filter};
use crate::tag_engine::producer::{ExtractCtx, Producer};
use crate::tag_engine::sanitize::AtomicChain;

/// The value a matching rule produces. `Const` holds any JSON literal (string, number, bool) so
/// the same rule table can back string classifiers (category ids, `road`), numeric ones (zoom),
/// and boolean ones (subsuming what used to be separate `FilterZoom`/`FilterMatch` producers).
/// `Producer` lets a rule's output be an arbitrary nested producer (e.g. an `Extract` with its own
/// `keys`/`sanitize`/`from`) — this is what lets `rules` subsume `cond`'s `then`/`else`: a
/// condition becomes a rule whose `when` is that condition and whose `value` is the `then`
/// producer, followed by an unconditional (`"when": true`) rule holding the `else` producer.
/// `Producer` must be tried before `Const`, since `Const(Value)` is an untagged catch-all that
/// would otherwise consume any object literal (including a producer's own JSON shape) first.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum ValueSpec {
    /// Copy a tag's own value (e.g. fall back to the raw `highway` value).
    Tag { tag: String },
    /// Copy `tag_or`'s value, or the literal `or` when the tag is absent.
    TagOr { tag_or: String, or: Value },
    Producer(Box<Producer>),
    /// A literal value.
    Const(Value),
}

impl ValueSpec {
    /// Resolve any nested `Producer`'s macros/sanitizers once at load time (`Tag`/`TagOr`/`Const`
    /// carry no named references, so they pass through unchanged).
    pub fn resolve(
        &self,
        macros: &HashMap<String, Filter>,
        sanitizers: &HashMap<String, AtomicChain>,
    ) -> anyhow::Result<ValueSpec> {
        Ok(match self {
            ValueSpec::Tag { tag } => ValueSpec::Tag { tag: tag.clone() },
            ValueSpec::TagOr { tag_or, or } => ValueSpec::TagOr { tag_or: tag_or.clone(), or: or.clone() },
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

#[derive(Debug, Clone, Deserialize)]
pub struct Classifier {
    pub rules: Vec<Rule>,
    /// Value to use when no rule matches — makes this table self-contained (no need for a
    /// wrapping `fallback`/category-const default) for cases like `minzoom` that always need
    /// a value.
    #[serde(default)]
    pub default: Option<Value>,
}

impl Classifier {
    /// First matching rule's value, or the table's `default`, or `None` if neither is set.
    pub fn classify(&self, ctx: &ExtractCtx) -> Option<Value> {
        classify_rules(&self.rules, ctx).or_else(|| self.default.clone())
    }
}

/// First matching rule's value, or `None` if no rule matches (first-match-wins). Shared by the
/// standalone `road` classifier and the data-defined `rules` value producer
/// (`tag_engine::producer`). Evaluated against a full `ExtractCtx` — same predicate evaluator
/// (`filter::eval`) and same context shape category matching uses, so a rule's `when` can see
/// side/prefix/infix/parent, not just raw tags. Does not apply a `default` — callers needing one
/// (e.g. `Classifier::classify`, `Producer::Classify`) apply it themselves.
pub fn classify_rules(rules: &[Rule], ctx: &ExtractCtx) -> Option<Value> {
    for rule in rules {
        if eval(&rule.when, ctx) {
            return match &rule.value {
                ValueSpec::Const(v) => Some(v.clone()),
                ValueSpec::Tag { tag } => ctx.obj_tags.get(tag).cloned().map(Value::String),
                ValueSpec::TagOr { tag_or, or } => Some(
                    ctx.obj_tags.get(tag_or).cloned().map(Value::String).unwrap_or_else(|| or.clone()),
                ),
                ValueSpec::Producer(p) => p.eval(ctx).map(|produced| produced.value),
            };
        }
    }
    None
}

// ── Shared classifier registry ──────────────────────────────────────────────

/// Shared, named classifiers loaded once from `<config_root>/producers.json`
/// (`{name: Classifier}`). Referenced from data via a `Classify`-style producer's
/// `{ "shared": "<name>" }`, so a rule table (e.g. the `road` classification) can be reused
/// across topics without duplication.
fn shared_classifiers() -> &'static HashMap<String, Classifier> {
    static CLASSIFIERS: OnceLock<HashMap<String, Classifier>> = OnceLock::new();
    CLASSIFIERS.get_or_init(|| {
        let path = crate::paths::config_root().join("producers.json");
        let raw = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
        serde_json::from_str(&raw).unwrap_or_else(|e| panic!("parsing {}: {e}", path.display()))
    })
}

/// The shared classifier registered under `name`, panicking if undefined (a config error).
pub fn shared_classifier(name: &str) -> &'static Classifier {
    shared_classifiers()
        .get(name)
        .unwrap_or_else(|| panic!("no shared classifier named '{name}' in <config_root>/producers.json"))
}
