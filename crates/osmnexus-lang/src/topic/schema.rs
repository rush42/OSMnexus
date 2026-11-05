//! The inferred output schema of a compiled topic: for every output field, either the closed set
//! of string values it can take or "any string". A compile result in its own right, so every
//! consumer (TS types today; TileJSON, JSON Schema and docs later) reads the same inference and
//! they can't disagree (see `boundaries_plan.md`, step 7). Moved here from `bin/export_ts_types`.
//!
//! A field whose every producer variant bottoms out in a `Sanitizer::Mapping` chain with
//! `on_miss: "drop"` (or absent, which defaults to drop — see `lang::sanitize::Sanitizer::Mapping`'s
//! own doc) is a closed set of string values, i.e. an enum. Anything else (an open-ended
//! sanitizer, a non-string mapping value, or a field with no closed source at all) is a plain
//! string — best-effort, not a precise model of every possible producer shape.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;

use crate::lang::producer::Producer;
use crate::lang::sanitize::{AtomicJson, Sanitizer};
use crate::topic::spec::Field;

/// The inferred type of one output field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FieldType {
    /// A closed set of string values (non-empty).
    Enum(BTreeSet<String>),
    /// Any string.
    String,
}

/// Every output field of a topic with its inferred type, in name order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OutputSchema {
    pub fields: BTreeMap<String, FieldType>,
}

/// What one producer variant contributes toward a field's type: a closed set of string values, or
/// "open" (no closed set — falls the whole field back to `String`).
enum Contribution {
    Closed(BTreeSet<String>),
    Open,
}

impl OutputSchema {
    /// Infer the schema from every producer variant of every field (topic defaults and every
    /// category's effective producers). Variants of the same output name are unioned.
    pub fn infer<'a>(fields: impl IntoIterator<Item = &'a Field>) -> Self {
        let mut contributions: BTreeMap<String, Vec<Contribution>> = BTreeMap::new();
        for field in fields {
            contributions.entry(field.output.clone()).or_default().push(collect(field));
        }
        let fields = contributions.into_iter().map(|(name, parts)| (name, unify(&parts))).collect();
        OutputSchema { fields }
    }
}

/// Union the contributions of every variant of a field: an enum if every variant contributed a
/// closed set (and the union is non-empty), `String` otherwise.
fn unify(parts: &[Contribution]) -> FieldType {
    if parts.iter().any(|c| matches!(c, Contribution::Open)) {
        return FieldType::String;
    }
    let mut values = BTreeSet::new();
    for part in parts {
        if let Contribution::Closed(set) = part {
            values.extend(set.iter().cloned());
        }
    }
    if values.is_empty() { FieldType::String } else { FieldType::Enum(values) }
}

/// Walk one field's `Producer` tree, collecting the closed value set (or lack thereof) of every
/// leaf's terminal sanitize-chain step, unioned into a single `Contribution` for this variant.
fn collect(field: &Field) -> Contribution {
    let mut values = BTreeSet::new();
    let mut open = false;
    walk(&field.source, &mut values, &mut open);
    if open { Contribution::Open } else { Contribution::Closed(values) }
}

fn walk(producer: &Producer, values: &mut BTreeSet<String>, open: &mut bool) {
    match producer {
        Producer::Extract { extract, .. } => match extract.sanitize().last() {
            None => *open = true, // identity read — unbounded raw tag value
            Some(Sanitizer::Mapping { mapping, on_miss }) if on_miss.is_none() || on_miss.as_deref() == Some("drop") => {
                for entry in mapping.values().flatten() {
                    match entry {
                        AtomicJson::Str(s) => {
                            values.insert(s.clone());
                        }
                        AtomicJson::Bool(_) | AtomicJson::Num(_) => *open = true,
                    }
                }
            }
            Some(_) => *open = true, // open-ended: Replace, Builtin, or Mapping with keep/constant on_miss
        },
        Producer::Match { rules, default, .. } => {
            for rule in rules {
                walk(&rule.value, values, open);
            }
            if let Some(Value::String(s)) = default {
                values.insert(s.clone());
            } else if default.is_some() {
                *open = true;
            }
        }
        Producer::Const { value, .. } => match value {
            Value::String(s) => {
                values.insert(s.clone());
            }
            _ => *open = true,
        },
        Producer::Parent(inner) => walk(inner, values, open),
    }
}
