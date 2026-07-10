//! A generic, data-driven classifier: an ordered list of `{ when, value }` rules, evaluated
//! against a way's tags with the shared `Filter` engine. The first rule whose condition matches
//! yields the value (any JSON literal); rules are first-match-wins, with an optional `default`.
//!
//! The value is either a literal (string/number/bool) or a `{ "tag": "<key>" }` passthrough that
//! copies the tag's own value (used e.g. to fall back to the raw `highway` value). All domain
//! knowledge lives in the JSON; this module is just the evaluator.

use std::collections::HashMap;
use std::sync::OnceLock;

use serde::Deserialize;
use serde_json::Value;

use crate::tag_engine::filter::{eval, Filter};
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
/// standalone `road` classifier and the data-defined `rules` value producer (`producer.rs`).
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

/// Shared, named classifiers loaded once from `topics/_shared/classifiers/<name>.json`
/// (name = file stem). Referenced from data via a `Classify`-style producer's `{ "shared": "<name>" }`,
/// so a rule table (e.g. the `road` classification) can be reused across topics without duplication.
fn shared_classifiers() -> &'static HashMap<String, Classifier> {
    static CLASSIFIERS: OnceLock<HashMap<String, Classifier>> = OnceLock::new();
    CLASSIFIERS.get_or_init(|| {
        let dir = crate::paths::shared_dir().join("classifiers");
        let dir = dir.display().to_string();
        let mut map = HashMap::new();
        let entries = std::fs::read_dir(&dir).unwrap_or_else(|e| panic!("reading {dir}: {e}"));
        for entry in entries {
            let path = entry.unwrap().path();
            if path.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            let name = path.file_stem().unwrap().to_string_lossy().to_string();
            let raw = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
            let classifier = serde_json::from_str(&raw)
                .unwrap_or_else(|e| panic!("parsing {}: {e}", path.display()));
            map.insert(name, classifier);
        }
        map
    })
}

/// The shared classifier registered under `name`, panicking if undefined (a config error).
pub fn shared_classifier(name: &str) -> &'static Classifier {
    shared_classifiers()
        .get(name)
        .unwrap_or_else(|| panic!("no shared classifier named '{name}' in topics/_shared/classifiers"))
}
