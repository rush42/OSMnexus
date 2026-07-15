//! The `Filter` predicate AST in two tiers: `FilterSpec` (as parsed from JSON — may carry `Macro`
//! nodes and named `sanitize:` references) and `Filter` (the resolved form `eval` operates on — no
//! `Macro` variant, `sanitize:` already resolved to a concrete `Sanitizer`). `FilterSpec::expand`
//! is the once-at-load pass that turns the former into the latter: it substitutes every `Macro`
//! node with its (recursively expanded) definition and resolves every `sanitize:` reference. This
//! module is thus purely the resolved evaluator (`eval`/`eval_filter`) plus that load-time pass —
//! `eval` never does a registry lookup of any kind, and the resolved `Filter` type makes an
//! unexpanded macro or unresolved sanitizer structurally impossible past load.
//!
//! Shares its context (`ExtractCtx`, in `lang::producer`) with `Producer::eval` — a predicate
//! is just another "object state → output" evaluator, output `bool` instead of `Option<Value>`. The
//! category *data model* and the priority-order compiler live in `categories`.

use std::collections::HashMap;

use serde::Deserialize;
use serde_json::Value;

use crate::lang::extract::Extract;
use crate::lang::producer::ExtractCtx;
use crate::lang::sanitize::{resolve_sanitize, Sanitizer, SanitizeRef};
use crate::osm::types::RawTags;
use crate::value_sets::value_set;

/// Filter expression, **as parsed from JSON** — may carry `Macro` nodes and named `sanitize:`
/// references, both resolved away by `expand` into the resolved `Filter` type below. Variants are
/// tried in declaration order by serde's untagged deserializer, so more-specific variants (those
/// with unique secondary fields) come before catch-alls.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum FilterSpec {
    /// A literal `true`/`false` — e.g. for topics (like osmnx's) whose whole filter already lives
    /// in `exclude_condition`, so a category just wants "match everything that reached here":
    /// `{ "condition": true }` instead of restating a tautological tag predicate.
    Bool(bool),

    // Combinators
    And { and: Vec<FilterSpec> },
    Or  { or:  Vec<FilterSpec> },
    Not { not: Box<FilterSpec> },
    /// Reference to a named macro defined in `macros.json` (topic-local or shared config-root). Only
    /// exists pre-`expand` — every resolved `Filter` reached by `eval` has had every `Macro` node
    /// substituted with its (recursively expanded) definition at load time (see `expand` below).
    Macro { r#macro: String },

    // Tag predicates — secondary field disambiguates (TagEq is the catch-all). Each flattens an
    // `Extract` (`tag`/`first_tag`, `Extract`'s JSON aliases for `key`/`keys` — see its own doc).
    // `sanitize` is a sibling field (not part of `Extract` itself — see its doc), and when set
    // normalizes the raw tag value before comparison.
    TagInSet     { #[serde(flatten)] extract: Extract, #[serde(default)] sanitize: Option<SanitizeRef>, in_set:      String      },
    TagIn        { #[serde(flatten)] extract: Extract, #[serde(default)] sanitize: Option<SanitizeRef>, r#in:        Vec<String> },
    TagContains  { #[serde(flatten)] extract: Extract, #[serde(default)] sanitize: Option<SanitizeRef>, contains: String, #[serde(default)] case_insensitive: bool },
    TagStartsWith{ #[serde(flatten)] extract: Extract, #[serde(default)] sanitize: Option<SanitizeRef>, starts_with: String      },
    TagEndsWith  { #[serde(flatten)] extract: Extract, #[serde(default)] sanitize: Option<SanitizeRef>, ends_with:   String      },
    TagExists    { #[serde(flatten)] extract: Extract, #[serde(default)] sanitize: Option<SanitizeRef>, exists:      bool        },
    TagEq        { #[serde(flatten)] extract: Extract, #[serde(default)] sanitize: Option<SanitizeRef>, eq:          String      },

    /// Evaluate the inner filter's `Tag*` predicates against the parent way's tags instead of the
    /// object's own — `false` when there is no parent.
    Parent { parent: Box<FilterSpec> },

    // Context predicates
    Side      { side:       String },   // "self" | "left" | "right"
    Prefix    { prefix:     String },
    Infix     { infix:      String },
    HasKeyPrefix { has_key_prefix: String },
    /// True iff the object has a parent way (i.e. it is a left/right side-split of a highway).
    HasParent { has_parent: bool },
    /// True iff the object's own tags are (non-)empty — total, unlike `TagExists`, which needs a
    /// specific key.
    TagsEmpty { tags_empty: bool },

    // Numeric comparisons. `num` names the tag to read, optionally run through a `sanitize` chain
    // first (which may yield a JSON number, e.g. `parse_length`) before parsing to f64.
    NumLt  { num: String, #[serde(default)] sanitize: Option<SanitizeRef>, lt:  f64 },
    NumLte { num: String, #[serde(default)] sanitize: Option<SanitizeRef>, lte: f64 },
    NumGt  { num: String, #[serde(default)] sanitize: Option<SanitizeRef>, gt:  f64 },
    NumGte { num: String, #[serde(default)] sanitize: Option<SanitizeRef>, gte: f64 },
}

/// Filter expression, **resolved** — every `Macro` node substituted and every `sanitize:` reference
/// resolved to a concrete `Sanitizer` (see `FilterSpec::expand`). This is what `eval` operates on;
/// it has no `Macro` variant and no named sanitizer reference, so neither can reach eval by
/// construction.
#[derive(Debug, Clone)]
pub enum Filter {
    Bool(bool),

    And { and: Vec<Filter> },
    Or  { or:  Vec<Filter> },
    Not { not: Box<Filter> },

    TagInSet     { extract: Extract, sanitize: Option<Sanitizer>, in_set:      String      },
    TagIn        { extract: Extract, sanitize: Option<Sanitizer>, r#in:        Vec<String> },
    TagContains  { extract: Extract, sanitize: Option<Sanitizer>, contains: String, case_insensitive: bool },
    TagStartsWith{ extract: Extract, sanitize: Option<Sanitizer>, starts_with: String      },
    TagEndsWith  { extract: Extract, sanitize: Option<Sanitizer>, ends_with:   String      },
    TagExists    { extract: Extract, sanitize: Option<Sanitizer>, exists:      bool        },
    TagEq        { extract: Extract, sanitize: Option<Sanitizer>, eq:          String      },

    Parent { parent: Box<Filter> },

    Side      { side:       String },
    Prefix    { prefix:     String },
    Infix     { infix:      String },
    HasKeyPrefix { has_key_prefix: String },
    HasParent { has_parent: bool },
    TagsEmpty { tags_empty: bool },

    NumLt  { num: String, sanitize: Option<Sanitizer>, lt:  f64 },
    NumLte { num: String, sanitize: Option<Sanitizer>, lte: f64 },
    NumGt  { num: String, sanitize: Option<Sanitizer>, gt:  f64 },
    NumGte { num: String, sanitize: Option<Sanitizer>, gte: f64 },
}

// ── Load-time resolution ──────────────────────────────────────────────────────

impl FilterSpec {
    /// Recursively resolve every named reference this `FilterSpec` (transitively) carries — `Macro`
    /// nodes (replaced by their expanded definition) and every `sanitize:` reference (resolved
    /// against `sanitizers`) — into the resolved `Filter` type, so `eval` never does a registry
    /// lookup of any kind. Called once at load time (see `topic::runner::TopicRunner::load`) with
    /// `macros` the topic's raw (still possibly macro-referencing) macro definitions, so a macro
    /// body referencing another macro is expanded recursively on demand.
    ///
    /// Hard-errors on an undefined macro/sanitizer name or a cyclic macro definition.
    pub fn expand(
        &self,
        macros: &HashMap<String, FilterSpec>,
        sanitizers: &HashMap<String, Sanitizer>,
    ) -> anyhow::Result<Filter> {
        self.expand_inner(macros, sanitizers, &mut Vec::new())
    }

    fn expand_inner(
        &self,
        macros: &HashMap<String, FilterSpec>,
        sanitizers: &HashMap<String, Sanitizer>,
        stack: &mut Vec<String>,
    ) -> anyhow::Result<Filter> {
        let resolve = |s: &Option<SanitizeRef>| -> anyhow::Result<Option<Sanitizer>> {
            s.as_ref().map(|r| r.resolve(sanitizers)).transpose()
        };
        Ok(match self {
            FilterSpec::And { and } =>
                Filter::And { and: and.iter().map(|f| f.expand_inner(macros, sanitizers, stack)).collect::<anyhow::Result<_>>()? },
            FilterSpec::Or { or } =>
                Filter::Or { or: or.iter().map(|f| f.expand_inner(macros, sanitizers, stack)).collect::<anyhow::Result<_>>()? },
            FilterSpec::Not { not } =>
                Filter::Not { not: Box::new(not.expand_inner(macros, sanitizers, stack)?) },
            FilterSpec::Macro { r#macro: name } => {
                if stack.iter().any(|n| n == name) {
                    stack.push(name.clone());
                    anyhow::bail!("cyclic macro definition: {}", stack.join(" -> "));
                }
                let def = macros.get(name)
                    .ok_or_else(|| anyhow::anyhow!("unknown macro: '{name}'"))?;
                stack.push(name.clone());
                let expanded = def.expand_inner(macros, sanitizers, stack)?;
                stack.pop();
                expanded
            }
            FilterSpec::Bool(b) => Filter::Bool(*b),
            FilterSpec::TagInSet { extract, sanitize, in_set } =>
                Filter::TagInSet { extract: extract.clone(), sanitize: resolve(sanitize)?, in_set: in_set.clone() },
            FilterSpec::TagIn { extract, sanitize, r#in } =>
                Filter::TagIn { extract: extract.clone(), sanitize: resolve(sanitize)?, r#in: r#in.clone() },
            FilterSpec::TagContains { extract, sanitize, contains, case_insensitive } =>
                Filter::TagContains { extract: extract.clone(), sanitize: resolve(sanitize)?, contains: contains.clone(), case_insensitive: *case_insensitive },
            FilterSpec::TagStartsWith { extract, sanitize, starts_with } =>
                Filter::TagStartsWith { extract: extract.clone(), sanitize: resolve(sanitize)?, starts_with: starts_with.clone() },
            FilterSpec::TagEndsWith { extract, sanitize, ends_with } =>
                Filter::TagEndsWith { extract: extract.clone(), sanitize: resolve(sanitize)?, ends_with: ends_with.clone() },
            FilterSpec::TagExists { extract, sanitize, exists } =>
                Filter::TagExists { extract: extract.clone(), sanitize: resolve(sanitize)?, exists: *exists },
            FilterSpec::TagEq { extract, sanitize, eq } =>
                Filter::TagEq { extract: extract.clone(), sanitize: resolve(sanitize)?, eq: eq.clone() },
            FilterSpec::Parent { parent } =>
                Filter::Parent { parent: Box::new(parent.expand_inner(macros, sanitizers, stack)?) },
            FilterSpec::Side { side } => Filter::Side { side: side.clone() },
            FilterSpec::Prefix { prefix } => Filter::Prefix { prefix: prefix.clone() },
            FilterSpec::Infix { infix } => Filter::Infix { infix: infix.clone() },
            FilterSpec::HasKeyPrefix { has_key_prefix } => Filter::HasKeyPrefix { has_key_prefix: has_key_prefix.clone() },
            FilterSpec::HasParent { has_parent } => Filter::HasParent { has_parent: *has_parent },
            FilterSpec::TagsEmpty { tags_empty } => Filter::TagsEmpty { tags_empty: *tags_empty },
            FilterSpec::NumLt { num, sanitize, lt } =>
                Filter::NumLt { num: num.clone(), sanitize: resolve(sanitize)?, lt: *lt },
            FilterSpec::NumLte { num, sanitize, lte } =>
                Filter::NumLte { num: num.clone(), sanitize: resolve(sanitize)?, lte: *lte },
            FilterSpec::NumGt { num, sanitize, gt } =>
                Filter::NumGt { num: num.clone(), sanitize: resolve(sanitize)?, gt: *gt },
            FilterSpec::NumGte { num, sanitize, gte } =>
                Filter::NumGte { num: num.clone(), sanitize: resolve(sanitize)?, gte: *gte },
        })
    }
}

impl Filter {
    /// A short, human-readable rendering (`surface == sett`, `a and b`, `highway in [...]`) — not
    /// the `Debug` dump, which is precise but unreadable at a glance (e.g. in the live editor's
    /// deriver tree view, `dag::render_producer`). Not meant to round-trip back into JSON or `eval`
    /// — purely for display.
    pub fn describe(&self) -> String {
        fn key(extract: &Extract) -> String {
            match extract {
                Extract::Value { key } => key.clone(),
                Extract::Candidates { keys } => format!("[{}]", keys.join("|")),
            }
        }
        // A predicate whose comparison already reads as a keyword (`in`, `contains`, ...) doesn't
        // need `(sanitized)` cluttering the common case — only flag it where it could silently
        // change what's being compared.
        fn maybe_sanitized(key: String, sanitize: &Option<Sanitizer>) -> String {
            if sanitize.is_some() { format!("{key} (sanitized)") } else { key }
        }
        match self {
            Filter::Bool(b) => b.to_string(),
            Filter::And { and } => and.iter().map(Filter::describe).collect::<Vec<_>>().join(" and "),
            Filter::Or { or } => format!("({})", or.iter().map(Filter::describe).collect::<Vec<_>>().join(" or ")),
            Filter::Not { not } => format!("not ({})", not.describe()),
            Filter::TagInSet { extract, sanitize, in_set } => format!("{} in {in_set}", maybe_sanitized(key(extract), sanitize)),
            Filter::TagIn { extract, sanitize, r#in } => format!("{} in [{}]", maybe_sanitized(key(extract), sanitize), r#in.join(", ")),
            Filter::TagContains { extract, sanitize, contains, case_insensitive } => format!(
                "{} contains {contains:?}{}",
                maybe_sanitized(key(extract), sanitize),
                if *case_insensitive { " (ci)" } else { "" },
            ),
            Filter::TagStartsWith { extract, sanitize, starts_with } => format!("{} starts_with {starts_with:?}", maybe_sanitized(key(extract), sanitize)),
            Filter::TagEndsWith { extract, sanitize, ends_with } => format!("{} ends_with {ends_with:?}", maybe_sanitized(key(extract), sanitize)),
            Filter::TagExists { extract, sanitize, exists } => {
                let k = maybe_sanitized(key(extract), sanitize);
                if *exists { format!("{k} exists") } else { format!("{k} !exists") }
            }
            Filter::TagEq { extract, sanitize, eq } => format!("{} == {eq:?}", maybe_sanitized(key(extract), sanitize)),
            Filter::Parent { parent } => format!("parent({})", parent.describe()),
            Filter::Side { side } => format!("side == {side:?}"),
            Filter::Prefix { prefix } => format!("prefix == {prefix:?}"),
            Filter::Infix { infix } => format!("infix == {infix:?}"),
            Filter::HasKeyPrefix { has_key_prefix } => format!("has_key_prefix({has_key_prefix:?})"),
            Filter::HasParent { has_parent } => if *has_parent { "has_parent".to_owned() } else { "!has_parent".to_owned() },
            Filter::TagsEmpty { tags_empty } => if *tags_empty { "tags_empty".to_owned() } else { "!tags_empty".to_owned() },
            Filter::NumLt { num, lt, .. } => format!("{num} < {lt}"),
            Filter::NumLte { num, lte, .. } => format!("{num} <= {lte}"),
            Filter::NumGt { num, gt, .. } => format!("{num} > {gt}"),
            Filter::NumGte { num, gte, .. } => format!("{num} >= {gte}"),
        }
    }
}

// ── Runtime evaluator ──────────────────────────────────────────────────────────

/// Evaluate `filter` against `ctx`. Shared by categorization and the way-level exclude check
/// (`eval_filter`, which builds a neutral `ctx`).
pub(crate) fn eval(filter: &Filter, ctx: &ExtractCtx) -> bool {
    match filter {
        Filter::Bool(b) => *b,
        Filter::And { and } => and.iter().all(|f| eval(f, ctx)),
        Filter::Or  { or  } => or.iter().any(|f| eval(f, ctx)),
        Filter::Not { not } => !eval(not, ctx),

        Filter::TagEq { extract, sanitize, eq } =>
            extract.read_str(sanitize.as_ref(), ctx.obj_tags).is_some_and(|v| v.as_ref() == eq.as_str()),
        Filter::TagInSet { extract, sanitize, in_set } =>
            extract.read_str(sanitize.as_ref(), ctx.obj_tags).is_some_and(|v| value_set(in_set).contains(v.as_ref())),
        Filter::TagIn { extract, sanitize, r#in } =>
            extract.read_str(sanitize.as_ref(), ctx.obj_tags).is_some_and(|v| r#in.iter().any(|s| s.as_str() == v.as_ref())),
        Filter::TagContains { extract, sanitize, contains, case_insensitive } =>
            extract.read_str(sanitize.as_ref(), ctx.obj_tags).is_some_and(|v| {
                if *case_insensitive {
                    v.to_lowercase().contains(contains.as_str())
                } else {
                    v.contains(contains.as_str())
                }
            }),
        Filter::TagStartsWith { extract, sanitize, starts_with } =>
            extract.read_str(sanitize.as_ref(), ctx.obj_tags).is_some_and(|v| v.starts_with(starts_with.as_str())),
        Filter::TagEndsWith { extract, sanitize, ends_with } =>
            extract.read_str(sanitize.as_ref(), ctx.obj_tags).is_some_and(|v| v.ends_with(ends_with.as_str())),
        Filter::TagExists { extract, sanitize, exists } =>
            extract.read_str(sanitize.as_ref(), ctx.obj_tags).is_some() == *exists,

        Filter::Parent { parent } => match ctx.parent_tags {
            None => false,
            Some(parent_tags) => eval(parent, &ExtractCtx { obj_tags: parent_tags, ..*ctx }),
        },

        // `_side` is always present in practice (`topic::pipeline::build_topic_rows` stamps it on every object,
        // self included) — defaulting to "self" here is a safety net, not the normal path.
        Filter::Side { side } =>
            ctx.annotations.get("_side").and_then(Value::as_str).unwrap_or("self") == side.as_str(),
        Filter::Prefix { prefix } => ctx.annotations.get("_prefix").and_then(Value::as_str) == Some(prefix.as_str()),
        Filter::Infix  { infix  } => ctx.annotations.get("_infix").and_then(Value::as_str)  == Some(infix.as_str()),
        Filter::HasKeyPrefix { has_key_prefix } =>
            ctx.obj_tags.keys().any(|k| k.starts_with(has_key_prefix.as_str())),
        // True iff there's a parent way (i.e. this is a left/right side-split object) —
        // `parent_tags` is only ever `Some` for those (see `categorize::transform::run_transform_steps`).
        Filter::HasParent { has_parent } => ctx.parent_tags.is_some() == *has_parent,
        Filter::TagsEmpty { tags_empty } => ctx.obj_tags.is_empty() == *tags_empty,

        Filter::NumLt  { num, sanitize, lt  } => read_num(ctx, num, sanitize.as_ref()).is_some_and(|n| n <  *lt),
        Filter::NumLte { num, sanitize, lte } => read_num(ctx, num, sanitize.as_ref()).is_some_and(|n| n <= *lte),
        Filter::NumGt  { num, sanitize, gt  } => read_num(ctx, num, sanitize.as_ref()).is_some_and(|n| n >  *gt),
        Filter::NumGte { num, sanitize, gte } => read_num(ctx, num, sanitize.as_ref()).is_some_and(|n| n >= *gte),
    }
}

/// Read a numeric value for a `num` predicate: reads tag `key` and, when `sanitize` is set, runs
/// it through that sanitizer chain (which may yield a JSON number, e.g. `parse_length`) before
/// coercing to f64. Returns None when the tag is absent or the value is unparseable — so every
/// numeric comparison is false on missing/garbage input. No geometry-derived values (length, …)
/// are available: classification is tag-only.
fn read_num(ctx: &ExtractCtx, key: &str, sanitize: Option<&Sanitizer>) -> Option<f64> {
    let raw = ctx.obj_tags.get(key)?;
    match sanitize {
        Some(_) => num_from_value(&resolve_sanitize(sanitize, raw)?),
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

/// Evaluate a Filter against raw tags with a neutral context (side=self, no parent).
/// Used by the topic engine for way-level exclude_condition checks.
pub fn eval_filter(filter: &Filter, tags: &RawTags) -> bool {
    let ctx = ExtractCtx {
        obj_tags: tags,
        parent_tags: None,
        id: "",
        annotations: crate::lang::producer::empty_annotations(),
    };
    eval(filter, &ctx)
}
