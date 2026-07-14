//! The `Filter` predicate AST, its load-time macro/sanitizer resolution (`expand`), and its
//! runtime evaluator (`eval`/`eval_filter`). Shares its context (`ExtractCtx`, in
//! `tag_engine::producer`) with `Producer::eval` — a predicate is just another "object state →
//! output" evaluator, output `bool` instead of `Option<Value>`. Neither `eval` nor `expand` does a
//! registry lookup at eval time: every named reference a `Filter` can carry (`Macro`, `sanitize:`)
//! is resolved once, up front, by `expand`. The category *data model* and the priority-order
//! compiler live in `categories`; this module is purely "given a context, does a predicate hold"
//! plus the load-time pass that gets it there.

use std::collections::HashMap;

use serde::Deserialize;

use crate::tag_engine::extract::Extract;
use crate::tag_engine::producer::ExtractCtx;
use crate::tag_engine::sanitize::{resolve_sanitize, Sanitizer, SanitizeRef};
use crate::tag_engine::transform::side_split::SplitContext;
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
    /// Reference to a named macro defined in `macros.json` (topic-local or shared config-root). Only exists
    /// pre-`expand` — every `Filter` actually reached by `eval` has had every `Macro` node
    /// substituted with its (recursively expanded) definition at load time (see `expand` below),
    /// so `eval`'s own `Macro` arm should never fire in practice.
    Macro { r#macro: String },

    // Tag predicates — secondary field disambiguates (TagEq is the catch-all). Each flattens an
    // `Extract` (`tag`/`first_tag`, `Extract`'s JSON aliases for `key`/`keys` — see its own doc),
    // so "first matching tag from a list" (what used to be a separate `FirstTag*` variant per
    // comparison) is just `Extract::Candidates` instead of `Extract::Value`; there's nothing else
    // that needs to differ. `sanitize` is a sibling field (not part of `Extract` itself — see its
    // doc), and when set normalizes the raw tag value before comparison (a dropped value behaves
    // as absent → false), mirroring the `num` predicate's `sanitize`.
    /// Membership in a named set from `<config_root>/value_sets.json` (keeps long value lists in data).
    TagInSet     { #[serde(flatten)] extract: Extract, #[serde(default)] sanitize: Option<SanitizeRef>, in_set:      String      },
    TagIn        { #[serde(flatten)] extract: Extract, #[serde(default)] sanitize: Option<SanitizeRef>, r#in:        Vec<String> },
    /// `case_insensitive` lower-cases both the tag value and `contains` before comparing — use it
    /// for free-text fields (notes/descriptions) where casing isn't meaningful. `contains` should
    /// already be written lowercase in JSON when this is set (only the tag value is lower-cased
    /// at eval time, to avoid re-lowering a literal on every call).
    TagContains  { #[serde(flatten)] extract: Extract, #[serde(default)] sanitize: Option<SanitizeRef>, contains: String, #[serde(default)] case_insensitive: bool },
    TagStartsWith{ #[serde(flatten)] extract: Extract, #[serde(default)] sanitize: Option<SanitizeRef>, starts_with: String      },
    TagEndsWith  { #[serde(flatten)] extract: Extract, #[serde(default)] sanitize: Option<SanitizeRef>, ends_with:   String      },
    /// With `sanitize` set, "exists" means "the value survives that sanitizer" (e.g. an
    /// unrecognized/garbage tag value counts as absent), not mere raw key presence — matching how
    /// a sided producer read (`first_present` + `sanitize`) decides whether it produced anything.
    TagExists    { #[serde(flatten)] extract: Extract, #[serde(default)] sanitize: Option<SanitizeRef>, exists:      bool        },
    TagEq        { #[serde(flatten)] extract: Extract, #[serde(default)] sanitize: Option<SanitizeRef>, eq:          String      },

    /// Evaluate the inner filter's `Tag*`/`FirstTag*` predicates against the parent way's tags
    /// instead of the object's own — `false` when there is no parent (matching the old
    /// `ParentTag*` predicates' behaviour). Composes freely (`and`/`or`/`not` of a `Parent`, or a
    /// `Parent` around any combination of tag predicates), unlike the one-off `ParentTag*` variants
    /// it replaces.
    Parent { parent: Box<Filter> },

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
    NumLt  { num: String, #[serde(default)] sanitize: Option<SanitizeRef>, lt:  f64 },
    NumLte { num: String, #[serde(default)] sanitize: Option<SanitizeRef>, lte: f64 },
    NumGt  { num: String, #[serde(default)] sanitize: Option<SanitizeRef>, gt:  f64 },
    NumGte { num: String, #[serde(default)] sanitize: Option<SanitizeRef>, gte: f64 },
}

// ── Load-time resolution ──────────────────────────────────────────────────────

impl Filter {
    /// Recursively resolve every named reference this `Filter` (transitively) carries — `Macro`
    /// nodes (replaced by their expanded definition) and every `sanitize:` reference (resolved
    /// against `sanitizers`, `SanitizeRef::resolve`) — so `eval` never does a registry lookup of
    /// any kind. Called once at load time on every `Filter` a topic owns (category `condition`s,
    /// `exclude_condition`, and any `when`/`cond` embedded in a `Producer` — see
    /// `Producer::resolve`) against `macros`, the topic's raw (also possibly macro-referencing)
    /// macro definitions.
    ///
    /// Hard-errors on an undefined macro/sanitizer name or a cyclic macro definition (`A`
    /// referencing `B` referencing `A`) rather than infinite-recursing — the same
    /// fail-fast-at-load philosophy as `CategoriesFile::build_order`'s `excludes` cycle check.
    pub fn expand(
        &self,
        macros: &HashMap<String, Filter>,
        sanitizers: &HashMap<String, Sanitizer>,
    ) -> anyhow::Result<Filter> {
        self.expand_inner(macros, sanitizers, &mut Vec::new())
    }

    fn expand_inner(
        &self,
        macros: &HashMap<String, Filter>,
        sanitizers: &HashMap<String, Sanitizer>,
        stack: &mut Vec<String>,
    ) -> anyhow::Result<Filter> {
        let resolve = |s: &Option<SanitizeRef>| -> anyhow::Result<Option<SanitizeRef>> {
            s.as_ref().map(|r| r.resolve(sanitizers)).transpose()
        };
        Ok(match self {
            Filter::And { and } =>
                Filter::And { and: and.iter().map(|f| f.expand_inner(macros, sanitizers, stack)).collect::<anyhow::Result<_>>()? },
            Filter::Or { or } =>
                Filter::Or { or: or.iter().map(|f| f.expand_inner(macros, sanitizers, stack)).collect::<anyhow::Result<_>>()? },
            Filter::Not { not } =>
                Filter::Not { not: Box::new(not.expand_inner(macros, sanitizers, stack)?) },
            Filter::Macro { r#macro: name } => {
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
            Filter::Bool(b) => Filter::Bool(*b),
            Filter::TagInSet { extract, sanitize, in_set } =>
                Filter::TagInSet { extract: extract.clone(), sanitize: resolve(sanitize)?, in_set: in_set.clone() },
            Filter::TagIn { extract, sanitize, r#in } =>
                Filter::TagIn { extract: extract.clone(), sanitize: resolve(sanitize)?, r#in: r#in.clone() },
            Filter::TagContains { extract, sanitize, contains, case_insensitive } =>
                Filter::TagContains { extract: extract.clone(), sanitize: resolve(sanitize)?, contains: contains.clone(), case_insensitive: *case_insensitive },
            Filter::TagStartsWith { extract, sanitize, starts_with } =>
                Filter::TagStartsWith { extract: extract.clone(), sanitize: resolve(sanitize)?, starts_with: starts_with.clone() },
            Filter::TagEndsWith { extract, sanitize, ends_with } =>
                Filter::TagEndsWith { extract: extract.clone(), sanitize: resolve(sanitize)?, ends_with: ends_with.clone() },
            Filter::TagExists { extract, sanitize, exists } =>
                Filter::TagExists { extract: extract.clone(), sanitize: resolve(sanitize)?, exists: *exists },
            Filter::TagEq { extract, sanitize, eq } =>
                Filter::TagEq { extract: extract.clone(), sanitize: resolve(sanitize)?, eq: eq.clone() },
            Filter::Parent { parent } =>
                Filter::Parent { parent: Box::new(parent.expand_inner(macros, sanitizers, stack)?) },
            Filter::Side { side } => Filter::Side { side: side.clone() },
            Filter::Prefix { prefix } => Filter::Prefix { prefix: prefix.clone() },
            Filter::Infix { infix } => Filter::Infix { infix: infix.clone() },
            Filter::HasKeyPrefix { has_key_prefix } => Filter::HasKeyPrefix { has_key_prefix: has_key_prefix.clone() },
            Filter::HasParent { has_parent } => Filter::HasParent { has_parent: *has_parent },
            Filter::NumLt { num, sanitize, lt } =>
                Filter::NumLt { num: num.clone(), sanitize: resolve(sanitize)?, lt: *lt },
            Filter::NumLte { num, sanitize, lte } =>
                Filter::NumLte { num: num.clone(), sanitize: resolve(sanitize)?, lte: *lte },
            Filter::NumGt { num, sanitize, gt } =>
                Filter::NumGt { num: num.clone(), sanitize: resolve(sanitize)?, gt: *gt },
            Filter::NumGte { num, sanitize, gte } =>
                Filter::NumGte { num: num.clone(), sanitize: resolve(sanitize)?, gte: *gte },
        })
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

        // Macros are expanded away at load time (`Filter::expand`) — a live tree should never
        // contain one.
        Filter::Macro { r#macro: name } => {
            tracing::error!("unexpanded macro '{name}' reached eval — Filter::expand should have run at load");
            false
        }

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

        Filter::Side { side } => ctx.split.obj_side == side.as_str(),
        Filter::Prefix    { prefix    } => ctx.split.prefix == Some(prefix.as_str()),
        Filter::Infix     { infix     } => ctx.split.infix  == Some(infix.as_str()),
        Filter::HasKeyPrefix { has_key_prefix } =>
            ctx.obj_tags.keys().any(|k| k.starts_with(has_key_prefix.as_str())),
        // True iff there's a parent way (i.e. this is a left/right side-split object) —
        // `parent_tags` is only ever `Some` for those (see `generate_sides`).
        Filter::HasParent { has_parent } => ctx.parent_tags.is_some() == *has_parent,

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
fn read_num(ctx: &ExtractCtx, key: &str, sanitize: Option<&SanitizeRef>) -> Option<f64> {
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
    let ctx = ExtractCtx { obj_tags: tags, parent_tags: None, split: SplitContext::default(), id: "" };
    eval(filter, &ctx)
}
