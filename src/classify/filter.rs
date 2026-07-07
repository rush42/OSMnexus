//! The `Filter` predicate AST and its evaluator, plus the categorization context. The category
//! *data model* and the priority-order compiler live in `categories`; this module is purely
//! "given a context, does a predicate hold".

use std::collections::HashMap;

use serde::Deserialize;

use crate::classify::sanitize::{first_present, SanitizerRegistry};
use crate::osm::types::RawTags;
use crate::output::types::Side;
use crate::value_sets::value_set;

/// Context passed to categorization predicates.
pub struct CategoryContext<'a> {
    pub tags: &'a RawTags,
    pub side: Side,
    pub prefix: Option<&'a str>,
    /// Original highway value of the parent way (set for left/right transformed objects).
    pub parent_highway: Option<&'a str>,
    /// Tags of the parent way (set for left/right transformed objects).
    pub parent_tags: Option<&'a RawTags>,
    /// The infix that matched during side splitting (e.g. "", "left", "both").
    pub infix: Option<&'a str>,
    /// Sanitizer registry — lets predicates normalize separation/traffic_mode via data.
    pub sanitizers: &'a SanitizerRegistry,
}

/// One conditional case in a `Producer::FilterZoom` rule list: evaluated with the same `Filter`
/// engine as every other producer, first match wins.
#[derive(Debug, Clone, Deserialize)]
pub struct MinzoomCase {
    pub when: Filter,
    pub zoom: i32,
}

/// Filter expression. Variants are tried in declaration order by serde's untagged deserializer,
/// so more-specific variants (those with unique secondary fields) come before catch-alls.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum Filter {
    /// A literal `true`/`false` — e.g. for topics (like osmnx's) whose whole filter already lives
    /// in `exclude_condition`, so a category just wants "match everything that reached here":
    /// `{ "condition": true }` instead of restating a tautological tag predicate.
    Bool(bool),

    // Combinators
    And { and: Vec<Filter> },
    Or  { or:  Vec<Filter> },
    Not { not: Box<Filter> },
    /// Reference to a named macro defined in `macros` or a Rust-implemented predicate.
    Macro { r#macro: String },

    // Tag predicates — secondary field disambiguates (TagEq is the catch-all).
    // The equality/membership predicates accept an optional `sanitize` chain: when set, the raw
    // tag value is normalized through that sanitizer before comparison (a dropped value behaves
    // as absent → false), mirroring the `num` predicate's `sanitize`.
    /// Membership in a named set from `_shared/value_sets.json` (keeps long value lists in data).
    TagInSet     { tag: String, in_set:      String      },
    TagIn        { tag: String, r#in:        Vec<String>, #[serde(default)] sanitize: Option<String> },
    TagContains  { tag: String, contains:    String      },
    TagStartsWith{ tag: String, starts_with: String      },
    TagEndsWith  { tag: String, ends_with:   String      },
    TagExists    { tag: String, exists:      bool        },
    TagEq        { tag: String, eq:          String,      #[serde(default)] sanitize: Option<String> },

    // "First matching tag from a list" — tries each key in order, uses the first that exists.
    /// First-present sibling of `TagInSet`; also honours an optional `sanitize` chain.
    FirstTagInSet { first_tag: Vec<String>, in_set: String, #[serde(default)] sanitize: Option<String> },
    FirstTagIn   { first_tag: Vec<String>, r#in:     Vec<String>, #[serde(default)] sanitize: Option<String> },
    FirstTagExists { first_tag: Vec<String>, exists: bool        },

    // Parent tag predicates
    ParentTagIn        { parent_tag: String, r#in:        Vec<String>, #[serde(default)] sanitize: Option<String> },
    ParentTagContains  { parent_tag: String, contains:    String      },
    ParentTagStartsWith{ parent_tag: String, starts_with: String      },
    ParentTagEndsWith  { parent_tag: String, ends_with:   String      },
    ParentTagEq        { parent_tag: String, eq:          String,      #[serde(default)] sanitize: Option<String> },

    // Context predicates
    Side      { side:       String },   // "self" | "left" | "right"
    Prefix    { prefix:     String },
    Infix     { infix:      String },
    HasKeyPrefix { has_key_prefix: String },
    /// True iff the object has a parent way (i.e. it is a left/right side-split of a highway).
    HasParent { has_parent: bool },

    // Numeric comparisons. `num` names the tag to read, optionally run through a `sanitize` chain
    // first (which may yield a JSON number, e.g. `parse_length`) before parsing to f64. Absent or
    // unparseable input makes the comparison false. The secondary field (lt/lte/gt/gte) is the op.
    // Geometry-derived values (length, area, …) are NOT available here — classification is
    // tag-only; length-based filtering is deferred to the geometry/graph stage.
    NumLt  { num: String, #[serde(default)] sanitize: Option<String>, lt:  f64 },
    NumLte { num: String, #[serde(default)] sanitize: Option<String>, lte: f64 },
    NumGt  { num: String, #[serde(default)] sanitize: Option<String>, gt:  f64 },
    NumGte { num: String, #[serde(default)] sanitize: Option<String>, gte: f64 },
}

// ── Filter evaluator ──────────────────────────────────────────────────────────

/// Evaluate `filter` against `ctx`. Shared by categorization and the way-level exclude check
/// (`eval_filter`, which builds a neutral `ctx`).
pub(crate) fn eval(filter: &Filter, ctx: &CategoryContext, macros: &HashMap<String, Filter>) -> bool {
    match filter {
        Filter::Bool(b) => *b,
        Filter::And { and } => and.iter().all(|f| eval(f, ctx, macros)),
        Filter::Or  { or  } => or.iter().any(|f| eval(f, ctx, macros)),
        Filter::Not { not } => !eval(not, ctx, macros),

        // JSON-defined macros (per-topic categories/macros.json + shared topics/_shared/).
        Filter::Macro { r#macro: name } => macros
            .get(name)
            .map(|f| eval(f, ctx, macros))
            .unwrap_or_else(|| { tracing::warn!("unknown macro: {}", name); false }),

        Filter::TagEq { tag, eq, sanitize } =>
            read_str(ctx.tags.get(tag).map(String::as_str), sanitize, ctx.sanitizers)
                .is_some_and(|v| v.as_ref() == eq.as_str()),
        Filter::TagInSet { tag, in_set } =>
            ctx.tags.get(tag).map(|v| value_set(in_set).contains(v)).unwrap_or(false),
        Filter::TagIn { tag, r#in, sanitize } =>
            read_str(ctx.tags.get(tag).map(String::as_str), sanitize, ctx.sanitizers)
                .is_some_and(|v| r#in.iter().any(|s| s.as_str() == v.as_ref())),
        Filter::TagContains { tag, contains } =>
            ctx.tags.get(tag).map(|v| v.contains(contains.as_str())).unwrap_or(false),
        Filter::TagStartsWith { tag, starts_with } =>
            ctx.tags.get(tag).map(|v| v.starts_with(starts_with.as_str())).unwrap_or(false),
        Filter::TagEndsWith { tag, ends_with } =>
            ctx.tags.get(tag).map(|v| v.ends_with(ends_with.as_str())).unwrap_or(false),
        Filter::TagExists { tag, exists } =>
            ctx.tags.contains_key(tag) == *exists,

        Filter::FirstTagInSet { first_tag, in_set, sanitize } =>
            read_str(first_present(ctx.tags, first_tag), sanitize, ctx.sanitizers)
                .is_some_and(|v| value_set(in_set).contains(v.as_ref())),
        Filter::FirstTagIn { first_tag, r#in, sanitize } =>
            read_str(first_present(ctx.tags, first_tag), sanitize, ctx.sanitizers)
                .is_some_and(|v| r#in.iter().any(|s| s.as_str() == v.as_ref())),
        Filter::FirstTagExists { first_tag, exists } =>
            first_tag.iter().any(|k| ctx.tags.contains_key(k)) == *exists,

        Filter::ParentTagEq { parent_tag, eq, sanitize } =>
            read_str(ctx.parent_tags.and_then(|t| t.get(parent_tag)).map(String::as_str), sanitize, ctx.sanitizers)
                .is_some_and(|v| v.as_ref() == eq.as_str()),
        Filter::ParentTagIn { parent_tag, r#in, sanitize } =>
            read_str(ctx.parent_tags.and_then(|t| t.get(parent_tag)).map(String::as_str), sanitize, ctx.sanitizers)
                .is_some_and(|v| r#in.iter().any(|s| s.as_str() == v.as_ref())),
        Filter::ParentTagContains { parent_tag, contains } =>
            ctx.parent_tags.and_then(|t| t.get(parent_tag))
                .map(|v| v.contains(contains.as_str()))
                .unwrap_or(false),
        Filter::ParentTagStartsWith { parent_tag, starts_with } =>
            ctx.parent_tags.and_then(|t| t.get(parent_tag))
                .map(|v| v.starts_with(starts_with.as_str()))
                .unwrap_or(false),
        Filter::ParentTagEndsWith { parent_tag, ends_with } =>
            ctx.parent_tags.and_then(|t| t.get(parent_tag))
                .map(|v| v.ends_with(ends_with.as_str()))
                .unwrap_or(false),

        Filter::Side { side } => {
            let s = match ctx.side { Side::Self_ => "self", Side::Left => "left", Side::Right => "right" };
            s == side.as_str()
        }
        Filter::Prefix    { prefix    } => ctx.prefix == Some(prefix.as_str()),
        Filter::Infix     { infix     } => ctx.infix  == Some(infix.as_str()),
        Filter::HasKeyPrefix { has_key_prefix } =>
            ctx.tags.keys().any(|k| k.starts_with(has_key_prefix.as_str())),
        Filter::HasParent { has_parent } => ctx.parent_highway.is_some() == *has_parent,

        Filter::NumLt  { num, sanitize, lt  } => read_num(ctx, num, sanitize).is_some_and(|n| n <  *lt),
        Filter::NumLte { num, sanitize, lte } => read_num(ctx, num, sanitize).is_some_and(|n| n <= *lte),
        Filter::NumGt  { num, sanitize, gt  } => read_num(ctx, num, sanitize).is_some_and(|n| n >  *gt),
        Filter::NumGte { num, sanitize, gte } => read_num(ctx, num, sanitize).is_some_and(|n| n >= *gte),
    }
}

/// Read a numeric value for a `num` predicate: reads tag `key` and, when `sanitize` is set, runs
/// it through that sanitizer chain (which may yield a JSON number, e.g. `parse_length`) before
/// coercing to f64. Returns None when the tag is absent or the value is unparseable — so every
/// numeric comparison is false on missing/garbage input. No geometry-derived values (length, …)
/// are available: classification is tag-only.
fn read_num(ctx: &CategoryContext, key: &str, sanitize: &Option<String>) -> Option<f64> {
    let raw = ctx.tags.get(key)?;
    match sanitize {
        Some(name) => num_from_value(&ctx.sanitizers.apply(name, raw)?),
        None => raw.trim().parse().ok(),
    }
}

/// Coerce an atomic sanitizer output to f64: a JSON number directly, or a string parsed as f64.
fn num_from_value(v: &serde_json::Value) -> Option<f64> {
    match v {
        serde_json::Value::Number(n) => n.as_f64(),
        serde_json::Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// Read a string value for a tag predicate. With no `sanitize`, returns the raw value borrowed;
/// with `sanitize`, runs it through that sanitizer chain (coercing the atomic output to a string),
/// yielding None when the value is dropped — so a sanitized-away value compares as absent.
/// Mirrors `read_num` for the numeric predicates.
fn read_str<'a>(
    raw: Option<&'a str>,
    sanitize: &Option<String>,
    reg: &SanitizerRegistry,
) -> Option<std::borrow::Cow<'a, str>> {
    let raw = raw?;
    match sanitize {
        None => Some(std::borrow::Cow::Borrowed(raw)),
        Some(name) => match reg.apply(name, raw)? {
            serde_json::Value::String(s) => Some(std::borrow::Cow::Owned(s)),
            other => other.as_str().map(|s| std::borrow::Cow::Owned(s.to_owned())),
        },
    }
}

/// Evaluate a Filter against raw tags with a neutral context (side=self, no parent).
/// Used by the topic engine for way-level exclude_condition checks.
pub fn eval_filter(
    filter: &Filter,
    tags: &RawTags,
    macros: &HashMap<String, Filter>,
    sanitizers: &SanitizerRegistry,
) -> bool {
    let ctx = CategoryContext {
        tags,
        side: Side::Self_,
        prefix: None,
        parent_highway: None,
        parent_tags: None,
        infix: None,
        sanitizers,
    };
    eval(filter, &ctx, macros)
}
