//! A generic, data-driven string classifier: an ordered list of `{ when, value }` rules,
//! evaluated against a way's tags with the shared `Filter` engine. The first rule whose
//! condition matches yields the value; rules are first-match-wins.
//!
//! The value is either a literal string or a `{ "tag": "<key>" }` passthrough that copies the
//! tag's own value (used e.g. to fall back to the raw `highway` value). All domain knowledge
//! lives in the JSON; this module is just the evaluator.

use std::collections::HashMap;
use std::sync::OnceLock;

use serde::Deserialize;

use crate::classify::categories::{eval_filter, Filter};
use crate::classify::sanitize::SanitizerRegistry;
use crate::osm::types::RawTags;

/// The value a matching rule produces.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum ValueSpec {
    /// Copy a tag's own value (e.g. fall back to the raw `highway` value).
    Tag { tag: String },
    /// A literal value.
    Const(String),
}

#[derive(Debug, Clone, Deserialize)]
pub struct Rule {
    pub when: Filter,
    pub value: ValueSpec,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Classifier {
    pub rules: Vec<Rule>,
}

impl Classifier {
    /// First matching rule's value, or `None` if no rule matches.
    pub fn classify(
        &self,
        tags: &RawTags,
        macros: &HashMap<String, Filter>,
        sanitizers: &SanitizerRegistry,
    ) -> Option<String> {
        classify_rules(&self.rules, tags, macros, sanitizers)
    }
}

/// First matching rule's value, or `None` if no rule matches (first-match-wins). Shared by the
/// standalone `road` classifier and the data-defined `rules` value producer (`engine/extract.rs`).
pub fn classify_rules(
    rules: &[Rule],
    tags: &RawTags,
    macros: &HashMap<String, Filter>,
    sanitizers: &SanitizerRegistry,
) -> Option<String> {
    for rule in rules {
        if eval_filter(&rule.when, tags, macros, sanitizers) {
            return match &rule.value {
                ValueSpec::Const(s) => Some(s.clone()),
                ValueSpec::Tag { tag } => tags.get(tag).cloned(),
            };
        }
    }
    None
}

/// The shared `road` classifier, loaded once from `topics/_shared/road_classification.json`.
pub fn road_classifier() -> &'static Classifier {
    static ROAD: OnceLock<Classifier> = OnceLock::new();
    ROAD.get_or_init(|| {
        const PATH: &str =
            concat!(env!("CARGO_MANIFEST_DIR"), "/topics/_shared/road_classification.json");
        let raw = std::fs::read_to_string(PATH).unwrap_or_else(|e| panic!("reading {PATH}: {e}"));
        serde_json::from_str(&raw).unwrap_or_else(|e| panic!("parsing {PATH}: {e}"))
    })
}
