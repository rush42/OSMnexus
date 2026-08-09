//! The `Filter` predicate AST: its runtime evaluator (`eval`/`eval_filter`). A `Filter` value never
//! carries a named macro or sanitizer reference — `{"macro": "<name>"}` and a bare
//! `"sanitize": "<name>"` are both resolved away as a `serde_json::Value`-tree rewrite
//! (`topic::load::inline_macro_refs`/`inline_sanitize_refs`) *before* any `Filter` JSON is
//! deserialized, so `eval` never does a registry lookup of any kind and an unexpanded macro or
//! unresolved sanitizer is structurally impossible here. `Deserialize` isn't derived directly —
//! `parser`'s hand-written impl disambiguates the untagged on-disk shapes (same treatment
//! `Producer` gets).
//!
//! Shares its context (`ExtractCtx`, in `lang::producer`) with `Producer::eval` — a predicate
//! is just another "object state → output" evaluator, output `bool` instead of `Option<Value>`. The
//! category *data model* and the priority-order compiler live in `categories`.

use std::ops::{Bound, RangeBounds};

use serde_json::Value;

use crate::lang::extract::Extract;
use crate::lang::producer::ExtractCtx;
use crate::osm::types::RawTags;
use crate::value_sets::value_set;

/// Filter expression. On-disk shapes are enumerated by `parser::FilterJson` (untagged, tried in
/// declaration order, more-specific/required-field shapes before catch-alls) rather than derived
/// directly here — see this module's own doc for why. No `Macro` variant — see this module's own
/// doc for where `{"macro": ...}` is resolved instead.
#[derive(Debug, Clone, PartialEq)]
pub enum Filter {
    /// A literal `true`/`false` — e.g. for topics (like osmnx's) whose whole filter already lives
    /// in `exclude_condition`, so a category just wants "match everything that reached here":
    /// `{ "condition": true }` instead of restating a tautological tag predicate.
    Bool(bool),

    // Combinators
    And { and: Vec<Filter> },
    Or  { or:  Vec<Filter> },
    Not { not: Box<Filter> },

    // Tag predicates — secondary field disambiguates (Eq is the catch-all). Each flattens an
    // `Extract` (`tag`/`first_tag`, `Extract`'s JSON aliases for `key`/`keys` — see its own doc),
    // whose own `sanitize` field (if set) normalizes the raw tag value before comparison.
    InSet     { extract: Extract, in_set:      String      },
    In        { extract: Extract, r#in:        Vec<String> },
    Contains  { extract: Extract, contains: String, case_insensitive: bool },
    StartsWith{ extract: Extract, starts_with: String      },
    EndsWith  { extract: Extract, ends_with:   String      },
    Exists    { extract: Extract, exists:      bool        },
    Eq        { extract: Extract, eq:          String      },

    /// Evaluate the inner filter's tag predicates against the parent way's tags instead of the
    /// object's own — `false` when there is no parent.
    Parent { parent: Box<Filter> },

    // Context predicates
    /// A plain `annotations[key] == eq` read, spelled directly in JSON as `{"annotation": key,
    /// "eq": value}` (`parser`) — `key` is typically one of `_side`/`_prefix`/`_infix`, whatever
    /// `topic::pipeline::build_topic_rules`/`CloneStep` stamps.
    AnnotationEq { key: String, eq: String },
    /// True iff some `obj_tags` key starts with `has_key_prefix` — a dynamic, unknown-suffix scan
    /// over the object's own raw tags, not an annotation read, so it can't fold into
    /// `AnnotationEq`.
    HasKeyPrefix { has_key_prefix: String },
    /// True iff the object has a parent way (i.e. it is a left/right side-split of a highway).
    HasParent { has_parent: bool },
    /// True iff the object's own tags are (non-)empty — total, unlike `Exists`, which needs a
    /// specific key.
    TagsEmpty { tags_empty: bool },

    // Numeric comparison — same `Extract` shape the tag predicates use (its `sanitize` chain may
    // yield a JSON number, e.g. `parse_length`) before parsing to f64. On-disk `lt`/`lte`/`gt`/`gte`
    // are sugar over this one interval shape (`parser`'s hand-written `Deserialize`) — each sets
    // just one bound, the other staying `Unbounded`. Internal only: no JSON shape spells `min`/`max`
    // directly. Represented as bounds (rather than four separate variants) so two numeric atoms on
    // the same key can be intersected directly (`categorize::linter`'s overlap lint) instead of
    // being treated as independent, opaque literals.
    NumRange { extract: Extract, min: Bound<f64>, max: Bound<f64> },
}

impl Filter {
    /// A short, human-readable rendering (`surface == sett`, `a and b`, `highway in [...]`) — not
    /// the `Debug` dump, which is precise but unreadable at a glance (e.g. in the live editor's
    /// deriver tree view, `tree::render_producer`). Not meant to round-trip back into JSON or `eval`
    /// — purely for display.
    pub fn describe(&self) -> String {
        fn key(extract: &Extract) -> String {
            match extract {
                Extract::Value { key, .. } => key.clone(),
                Extract::Candidates { keys, .. } => format!("[{}]", keys.join("|")),
            }
        }
        // A predicate whose comparison already reads as a keyword (`in`, `contains`, ...) doesn't
        // need `(sanitized)` cluttering the common case — only flag it where it could silently
        // change what's being compared.
        fn maybe_sanitized(extract: &Extract) -> String {
            let k = key(extract);
            if !extract.sanitize().is_empty() { format!("{k} (sanitized)") } else { k }
        }
        match self {
            Filter::Bool(b) => b.to_string(),
            Filter::And { and } => and.iter().map(Filter::describe).collect::<Vec<_>>().join("\nand\n"),
            Filter::Or { or } => format!("({})", or.iter().map(Filter::describe).collect::<Vec<_>>().join("\nor\n")),
            Filter::Not { not } => format!("not ({})", not.describe()),
            Filter::InSet { extract, in_set } => format!("{} in {in_set}", maybe_sanitized(extract)),
            Filter::In { extract, r#in } => format!("{} in [{}]", maybe_sanitized(extract), r#in.join(", ")),
            Filter::Contains { extract, contains, case_insensitive } => format!(
                "{} contains {contains:?}{}",
                maybe_sanitized(extract),
                if *case_insensitive { " (ci)" } else { "" },
            ),
            Filter::StartsWith { extract, starts_with } => format!("{} starts_with {starts_with:?}", maybe_sanitized(extract)),
            Filter::EndsWith { extract, ends_with } => format!("{} ends_with {ends_with:?}", maybe_sanitized(extract)),
            Filter::Exists { extract, exists } => {
                let k = maybe_sanitized(extract);
                if *exists { format!("{k} exists") } else { format!("{k} !exists") }
            }
            Filter::Eq { extract, eq } => format!("{} == {eq:?}", maybe_sanitized(extract)),
            Filter::Parent { parent } => format!("parent({})", parent.describe()),
            Filter::AnnotationEq { key, eq } => format!("{key} == {eq:?}"),
            Filter::HasKeyPrefix { has_key_prefix } => format!("has_key_prefix({has_key_prefix:?})"),
            Filter::HasParent { has_parent } => if *has_parent { "has_parent".to_owned() } else { "!has_parent".to_owned() },
            Filter::TagsEmpty { tags_empty } => if *tags_empty { "tags_empty".to_owned() } else { "!tags_empty".to_owned() },
            Filter::NumRange { extract, min, max } => {
                let k = key(extract);
                match (min, max) {
                    (Bound::Unbounded, Bound::Unbounded) => k,
                    (Bound::Included(a), Bound::Unbounded) => format!("{k} >= {a}"),
                    (Bound::Excluded(a), Bound::Unbounded) => format!("{k} > {a}"),
                    (Bound::Unbounded, Bound::Included(b)) => format!("{k} <= {b}"),
                    (Bound::Unbounded, Bound::Excluded(b)) => format!("{k} < {b}"),
                    _ => format!("{k} in {min:?}..{max:?}"),
                }
            }
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

        Filter::Eq { extract, eq } =>
            extract.read_str(ctx).is_some_and(|v| v.as_ref() == eq.as_str()),
        Filter::InSet { extract, in_set } =>
            extract.read_str(ctx).is_some_and(|v| value_set(in_set).contains(v.as_ref())),
        Filter::In { extract, r#in } =>
            extract.read_str(ctx).is_some_and(|v| r#in.iter().any(|s| s.as_str() == v.as_ref())),
        Filter::Contains { extract, contains, case_insensitive } =>
            extract.read_str(ctx).is_some_and(|v| {
                if *case_insensitive {
                    v.to_lowercase().contains(contains.as_str())
                } else {
                    v.contains(contains.as_str())
                }
            }),
        Filter::StartsWith { extract, starts_with } =>
            extract.read_str(ctx).is_some_and(|v| v.starts_with(starts_with.as_str())),
        Filter::EndsWith { extract, ends_with } =>
            extract.read_str(ctx).is_some_and(|v| v.ends_with(ends_with.as_str())),
        Filter::Exists { extract, exists } =>
            extract.read_str(ctx).is_some() == *exists,

        Filter::Parent { parent } => match ctx.parent_tags {
            None => false,
            Some(parent_tags) => eval(parent, &ExtractCtx { obj_tags: parent_tags, ..*ctx }),
        },

        // `_side` is absent on a base object and means "self" there (see
        // `lang::producer::side_of`), so it needs that default applied before comparing.
        // `_prefix`/`_infix` have no such default — they only ever exist on a side-split object, and
        // absent genuinely means "no prefix/infix", so an absent key compares unequal.
        Filter::AnnotationEq { key, eq } if key == "_side" =>
            crate::lang::producer::side_of(ctx) == eq.as_str(),
        Filter::AnnotationEq { key, eq } =>
            ctx.annotations.get(key).and_then(Value::as_str) == Some(eq.as_str()),
        Filter::HasKeyPrefix { has_key_prefix } =>
            ctx.obj_tags.keys().any(|k| k.starts_with(has_key_prefix.as_str())),
        // True iff there's a parent way (i.e. this is a left/right side-split object) —
        // `parent_tags` is only ever `Some` for those (see `categorize::transform::run_transform_steps`).
        Filter::HasParent { has_parent } => ctx.parent_tags.is_some() == *has_parent,
        Filter::TagsEmpty { tags_empty } => ctx.obj_tags.is_empty() == *tags_empty,

        Filter::NumRange { extract, min, max } =>
            read_num(extract, ctx).is_some_and(|n| (*min, *max).contains(&n)),
    }
}

/// Read a numeric value for a `num` predicate: reads `extract` (single key or first-present
/// candidate list, same as the tag predicates) and, when its `sanitize` is set, runs it through
/// that sanitizer chain (which may yield a JSON number, e.g. `parse_length`) before coercing to
/// f64. Returns None when the tag is absent or the value is unparseable — so every numeric
/// comparison is false on missing/garbage input. No geometry-derived values (length, …) are
/// available: classification is tag-only.
pub(crate) fn read_num(extract: &Extract, ctx: &ExtractCtx) -> Option<f64> {
    if extract.sanitize().is_empty() {
        extract.read_raw(ctx)?.trim().parse().ok()
    } else {
        num_from_value(&extract.read(ctx)?)
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
pub fn eval_filter(filter: &Filter, tags: &RawTags<'_>) -> bool {
    let ctx = ExtractCtx {
        obj_tags: tags,
        parent_tags: None,
        id: "",
        annotations: crate::lang::producer::empty_annotations(),
    };
    eval(filter, &ctx)
}
