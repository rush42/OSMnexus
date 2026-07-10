//! The `Filter` predicate AST and its evaluator. Shares its context/lookup-table pair
//! (`ExtractCtx`/`Env`, both in `producer.rs`) with `Producer::eval` — a predicate is just another
//! "(object state, lookup tables) → output" evaluator, output `bool` instead of `Option<Value>`.
//! The category *data model* and the priority-order compiler live in `categories`; this module is
//! purely "given a context, does a predicate hold".

use std::collections::HashMap;

use serde::Deserialize;

use crate::tag_engine::keys::first_present;
use crate::tag_engine::producer::{resolve_sanitize, AtomicChain, Env, ExtractCtx};
use crate::osm::types::RawTags;
use crate::value_sets::value_set;

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
    /// Reference to a named macro defined in `macros.json`/`_shared/macros/`. Only exists
    /// pre-`expand` — every `Filter` actually reached by `eval` has had every `Macro` node
    /// substituted with its (recursively expanded) definition at load time (see `expand` below),
    /// so `eval`'s own `Macro` arm should never fire in practice.
    Macro { r#macro: String },

    // Tag predicates — secondary field disambiguates (TagEq is the catch-all).
    // The equality/membership predicates accept an optional `sanitize` chain: when set, the raw
    // tag value is normalized through that sanitizer before comparison (a dropped value behaves
    // as absent → false), mirroring the `num` predicate's `sanitize`.
    /// Membership in a named set from `_shared/value_sets.json` (keeps long value lists in data).
    TagInSet     { tag: String, in_set:      String      },
    TagIn        { tag: String, r#in:        Vec<String>, #[serde(default)] sanitize: Option<String> },
    /// `case_insensitive` lower-cases both the tag value and `contains` before comparing — use it
    /// for free-text fields (notes/descriptions) where casing isn't meaningful. `contains` should
    /// already be written lowercase in JSON when this is set (only the tag value is lower-cased
    /// at eval time, to avoid re-lowering a literal on every call).
    TagContains  { tag: String, contains: String, #[serde(default)] case_insensitive: bool },
    TagStartsWith{ tag: String, starts_with: String      },
    TagEndsWith  { tag: String, ends_with:   String      },
    TagExists    { tag: String, exists:      bool        },
    TagEq        { tag: String, eq:          String,      #[serde(default)] sanitize: Option<String> },

    // "First matching tag from a list" — tries each key in order, uses the first that exists.
    /// First-present sibling of `TagInSet`; also honours an optional `sanitize` chain.
    FirstTagInSet { first_tag: Vec<String>, in_set: String, #[serde(default)] sanitize: Option<String> },
    FirstTagIn   { first_tag: Vec<String>, r#in:     Vec<String>, #[serde(default)] sanitize: Option<String> },
    /// With `sanitize` set, "exists" means "the first-present candidate's value survives that
    /// sanitizer" (e.g. an unrecognized/garbage tag value counts as absent), not mere raw key
    /// presence — matching how a sided producer read (`first_present` + `sanitize`) decides
    /// whether it produced anything.
    FirstTagExists { first_tag: Vec<String>, exists: bool, #[serde(default)] sanitize: Option<String> },

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

impl Filter {
    /// Recursively replace every `Macro` node with its (recursively expanded) definition, so
    /// `eval` never has to do a live macro lookup. Called once at load time on every `Filter` a
    /// topic owns (category `condition`s, `exclude_condition`, and any `when`/`cond` embedded in
    /// a `Producer` — see `Producer::expand_macros`) against `macros`, the topic's raw (also
    /// possibly macro-referencing) macro definitions.
    ///
    /// Hard-errors on an undefined macro name or a cyclic macro definition (`A` referencing `B`
    /// referencing `A`) rather than infinite-recursing — the same fail-fast-at-load philosophy as
    /// `CategoriesFile::build_order`'s `excludes` cycle check.
    pub fn expand(&self, macros: &HashMap<String, Filter>) -> anyhow::Result<Filter> {
        self.expand_inner(macros, &mut Vec::new())
    }

    fn expand_inner(&self, macros: &HashMap<String, Filter>, stack: &mut Vec<String>) -> anyhow::Result<Filter> {
        Ok(match self {
            Filter::And { and } =>
                Filter::And { and: and.iter().map(|f| f.expand_inner(macros, stack)).collect::<anyhow::Result<_>>()? },
            Filter::Or { or } =>
                Filter::Or { or: or.iter().map(|f| f.expand_inner(macros, stack)).collect::<anyhow::Result<_>>()? },
            Filter::Not { not } =>
                Filter::Not { not: Box::new(not.expand_inner(macros, stack)?) },
            Filter::Macro { r#macro: name } => {
                if stack.iter().any(|n| n == name) {
                    stack.push(name.clone());
                    anyhow::bail!("cyclic macro definition: {}", stack.join(" -> "));
                }
                let def = macros.get(name)
                    .ok_or_else(|| anyhow::anyhow!("unknown macro: '{name}'"))?;
                stack.push(name.clone());
                let expanded = def.expand_inner(macros, stack)?;
                stack.pop();
                expanded
            }
            other => other.clone(),
        })
    }
}

// ── Filter evaluator ──────────────────────────────────────────────────────────

/// Evaluate `filter` against `ctx`/`env`. Shared by categorization and the way-level exclude check
/// (`eval_filter`, which builds a neutral `ctx`).
pub(crate) fn eval(filter: &Filter, ctx: &ExtractCtx, env: &Env) -> bool {
    match filter {
        Filter::Bool(b) => *b,
        Filter::And { and } => and.iter().all(|f| eval(f, ctx, env)),
        Filter::Or  { or  } => or.iter().any(|f| eval(f, ctx, env)),
        Filter::Not { not } => !eval(not, ctx, env),

        // Macros are expanded away at load time (`Filter::expand`) — a live tree should never
        // contain one; `Env` doesn't even carry a macro table to resolve this against.
        Filter::Macro { r#macro: name } => {
            tracing::error!("unexpanded macro '{name}' reached eval — Filter::expand should have run at load");
            false
        }

        Filter::TagEq { tag, eq, sanitize } =>
            read_str(ctx.obj_tags.get(tag).map(String::as_str), sanitize, env.sanitizers)
                .is_some_and(|v| v.as_ref() == eq.as_str()),
        Filter::TagInSet { tag, in_set } =>
            ctx.obj_tags.get(tag).map(|v| value_set(in_set).contains(v)).unwrap_or(false),
        Filter::TagIn { tag, r#in, sanitize } =>
            read_str(ctx.obj_tags.get(tag).map(String::as_str), sanitize, env.sanitizers)
                .is_some_and(|v| r#in.iter().any(|s| s.as_str() == v.as_ref())),
        Filter::TagContains { tag, contains, case_insensitive } =>
            ctx.obj_tags.get(tag).map(|v| {
                if *case_insensitive {
                    v.to_lowercase().contains(contains.as_str())
                } else {
                    v.contains(contains.as_str())
                }
            }).unwrap_or(false),
        Filter::TagStartsWith { tag, starts_with } =>
            ctx.obj_tags.get(tag).map(|v| v.starts_with(starts_with.as_str())).unwrap_or(false),
        Filter::TagEndsWith { tag, ends_with } =>
            ctx.obj_tags.get(tag).map(|v| v.ends_with(ends_with.as_str())).unwrap_or(false),
        Filter::TagExists { tag, exists } =>
            ctx.obj_tags.contains_key(tag) == *exists,

        Filter::FirstTagInSet { first_tag, in_set, sanitize } =>
            read_str(first_present(ctx.obj_tags, first_tag), sanitize, env.sanitizers)
                .is_some_and(|v| value_set(in_set).contains(v.as_ref())),
        Filter::FirstTagIn { first_tag, r#in, sanitize } =>
            read_str(first_present(ctx.obj_tags, first_tag), sanitize, env.sanitizers)
                .is_some_and(|v| r#in.iter().any(|s| s.as_str() == v.as_ref())),
        Filter::FirstTagExists { first_tag, exists, sanitize: None } =>
            first_tag.iter().any(|k| ctx.obj_tags.contains_key(k)) == *exists,
        Filter::FirstTagExists { first_tag, exists, sanitize: Some(name) } =>
            read_str(first_present(ctx.obj_tags, first_tag), &Some(name.clone()), env.sanitizers).is_some() == *exists,

        Filter::ParentTagEq { parent_tag, eq, sanitize } =>
            read_str(ctx.parent_tags.and_then(|t| t.get(parent_tag)).map(String::as_str), sanitize, env.sanitizers)
                .is_some_and(|v| v.as_ref() == eq.as_str()),
        Filter::ParentTagIn { parent_tag, r#in, sanitize } =>
            read_str(ctx.parent_tags.and_then(|t| t.get(parent_tag)).map(String::as_str), sanitize, env.sanitizers)
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

        Filter::Side { side } => ctx.obj_side == side.as_str(),
        Filter::Prefix    { prefix    } => ctx.prefix == Some(prefix.as_str()),
        Filter::Infix     { infix     } => ctx.infix  == Some(infix.as_str()),
        Filter::HasKeyPrefix { has_key_prefix } =>
            ctx.obj_tags.keys().any(|k| k.starts_with(has_key_prefix.as_str())),
        // True iff there's a parent way (i.e. this is a left/right side-split object) —
        // `parent_tags` is only ever `Some` for those (see `get_transformed_objects`).
        Filter::HasParent { has_parent } => ctx.parent_tags.is_some() == *has_parent,

        Filter::NumLt  { num, sanitize, lt  } => read_num(ctx, env, num, sanitize).is_some_and(|n| n <  *lt),
        Filter::NumLte { num, sanitize, lte } => read_num(ctx, env, num, sanitize).is_some_and(|n| n <= *lte),
        Filter::NumGt  { num, sanitize, gt  } => read_num(ctx, env, num, sanitize).is_some_and(|n| n >  *gt),
        Filter::NumGte { num, sanitize, gte } => read_num(ctx, env, num, sanitize).is_some_and(|n| n >= *gte),
    }
}

/// Read a numeric value for a `num` predicate: reads tag `key` and, when `sanitize` is set, runs
/// it through that sanitizer chain (which may yield a JSON number, e.g. `parse_length`) before
/// coercing to f64. Returns None when the tag is absent or the value is unparseable — so every
/// numeric comparison is false on missing/garbage input. No geometry-derived values (length, …)
/// are available: classification is tag-only.
fn read_num(ctx: &ExtractCtx, env: &Env, key: &str, sanitize: &Option<String>) -> Option<f64> {
    let raw = ctx.obj_tags.get(key)?;
    match sanitize {
        Some(name) => num_from_value(&resolve_sanitize(env.sanitizers, Some(name.as_str()), raw)?),
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
    reg: &HashMap<String, AtomicChain>,
) -> Option<std::borrow::Cow<'a, str>> {
    let raw = raw?;
    match sanitize {
        None => Some(std::borrow::Cow::Borrowed(raw)),
        Some(name) => match resolve_sanitize(reg, Some(name.as_str()), raw)? {
            serde_json::Value::String(s) => Some(std::borrow::Cow::Owned(s)),
            other => other.as_str().map(|s| std::borrow::Cow::Owned(s.to_owned())),
        },
    }
}

/// Evaluate a Filter against raw tags with a neutral context (side=self, no parent).
/// Used by the topic engine for way-level exclude_condition checks.
pub fn eval_filter(filter: &Filter, tags: &RawTags, env: &Env) -> bool {
    let ctx = ExtractCtx {
        obj_tags: tags,
        parent_tags: None,
        obj_side: "self",
        prefix: None,
        infix: None,
    };
    eval(filter, &ctx, env)
}
