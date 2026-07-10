//! A generic, data-driven classifier: an ordered list of `{ when, value }` rules, evaluated
//! against a way's tags with the shared `Filter` engine. The first rule whose condition matches
//! yields the value (any JSON literal); rules are first-match-wins, with an optional `default`.
//!
//! The value is either a literal (string/number/bool) or a `{ "tag": "<key>" }` passthrough that
//! copies the tag's own value (used e.g. to fall back to the raw `highway` value). All domain
//! knowledge lives in the JSON; this module is just the evaluator.

use serde::Deserialize;
use serde_json::Value;

use crate::tag_engine::producer::filter::{eval, Filter};
use crate::tag_engine::producer::ExtractCtx;

/// The value a matching rule produces. `Const` holds any JSON literal (string, number, bool) so
/// the same rule table can back string classifiers (category ids, `road`), numeric ones (zoom),
/// and boolean ones (subsuming what used to be separate `FilterZoom`/`FilterMatch` producers).
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum ValueSpec {
    /// Copy a tag's own value (e.g. fall back to the raw `highway` value).
    Tag { tag: String },
    /// Copy `tag_or`'s value, or the literal `or` when the tag is absent.
    TagOr { tag_or: String, or: Value },
    /// A literal value.
    Const(Value),
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
/// standalone `road` classifier and the data-defined `rules` value producer (`producer/mod.rs`).
/// Evaluated against a full `ExtractCtx` — same predicate evaluator (`filter::eval`) and same
/// context shape category matching uses, so a rule's `when` can see side/prefix/infix/parent, not
/// just raw tags. Does not apply a `default` — callers needing one (e.g. `Classifier::classify`,
/// `Producer::Classify`) apply it themselves.
pub fn classify_rules(rules: &[Rule], ctx: &ExtractCtx) -> Option<Value> {
    for rule in rules {
        if eval(&rule.when, ctx) {
            return match &rule.value {
                ValueSpec::Const(v) => Some(v.clone()),
                ValueSpec::Tag { tag } => ctx.obj_tags.get(tag).cloned().map(Value::String),
                ValueSpec::TagOr { tag_or, or } => Some(
                    ctx.obj_tags.get(tag_or).cloned().map(Value::String).unwrap_or_else(|| or.clone()),
                ),
            };
        }
    }
    None
}
