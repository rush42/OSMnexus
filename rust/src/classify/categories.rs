use std::collections::HashMap;

use anyhow::Context;
use serde::Deserialize;

use crate::engine::topic::DeriverBinding;
use crate::osm::types::RawTags;
use crate::output::types::Side;
use crate::classify::sanitize::SanitizerRegistry;
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
    pub length_m: f64,
    /// Sanitizer registry — lets predicates normalize separation/traffic_mode via data.
    pub sanitizers: &'a SanitizerRegistry,
}

// ── Data types ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
pub struct CategoryDef {
    pub id: String,
    pub condition: Filter,
    pub excludes: Option<Vec<String>>,
    /// Per-category minzoom override. Falls back to the topic-level default when absent.
    #[serde(default)]
    pub minzoom: Option<MinzoomRule>,
    /// Per-category deriver overrides: re-bind a different deriver to an output (replacing the
    /// topic default for that output). E.g. surface/smoothness sourced from the parent highway.
    #[serde(default)]
    pub derivers: Option<Vec<DeriverBinding>>,
    /// Per-category constants (override the topic-level `consts` per key). Seeded into `derived`
    /// as the lowest-priority layer; a sanitizer/deriver producing the same key overrides them.
    #[serde(default)]
    pub consts: serde_json::Map<String, serde_json::Value>,
    /// Per-category private metadata (override the topic-level `private` per key). Emitted into the
    /// `private` output column verbatim — the explicit counterpart to `consts`, for internal keys
    /// like `_implicit_oneway_confidence` that are not part of the public `derived` payload.
    #[serde(default)]
    pub private: serde_json::Map<String, serde_json::Value>,
    /// Scope of parking-based `traffic_mode` inference for this category: `"both"` (infer both
    /// sides, e.g. bicycle roads) or `"directional"` (infer only the transformed side, e.g.
    /// on-highway cycleway lanes). Absent = no parking inference.
    #[serde(default)]
    pub parking_inference: Option<String>,
}

/// Declarative minzoom: a constant, or an ordered list of conditional cases with a default.
/// Cases are evaluated in order against the same `CategoryContext` used for categorization,
/// reusing the `Filter` evaluator; the first matching case wins, else `default`.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum MinzoomRule {
    Const(i32),
    Conditional { default: i32, rules: Vec<MinzoomCase> },
}

#[derive(Debug, Clone, Deserialize)]
pub struct MinzoomCase {
    pub when: Filter,
    pub zoom: i32,
}

/// Resolve a minzoom rule against a categorization context.
pub fn resolve_minzoom(
    rule: &MinzoomRule,
    ctx: &CategoryContext,
    macros: &HashMap<String, Filter>,
) -> i32 {
    match rule {
        MinzoomRule::Const(z) => *z,
        MinzoomRule::Conditional { default, rules } => rules
            .iter()
            .find(|case| eval(&case.when, ctx, macros))
            .map(|case| case.zoom)
            .unwrap_or(*default),
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct CategoriesFile {
    pub macros: HashMap<String, Filter>,
    pub categories: Vec<CategoryDef>,
}

/// Filter expression. Variants are tried in declaration order by serde's untagged deserializer,
/// so more-specific variants (those with unique secondary fields) come before catch-alls.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum Filter {
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

    // Numeric comparisons. `num` names the value: the reserved key `"length"` reads the context
    // length in metres; any other key reads that tag, optionally run through a `sanitize` chain
    // first (which may yield a JSON number, e.g. `parse_length`) before parsing to f64. Absent or
    // unparseable input makes the comparison false. The secondary field (lt/lte/gt/gte) is the op.
    NumLt  { num: String, #[serde(default)] sanitize: Option<String>, lt:  f64 },
    NumLte { num: String, #[serde(default)] sanitize: Option<String>, lte: f64 },
    NumGt  { num: String, #[serde(default)] sanitize: Option<String>, gt:  f64 },
    NumGte { num: String, #[serde(default)] sanitize: Option<String>, gte: f64 },
}

// ── Category loader ───────────────────────────────────────────────────────────

/// Load a topic's categories from its `categories/` directory.
/// Reads `macros.json` (optional) + all other `*.json` files (sorted), injecting the
/// category `id` from each file stem.
pub fn load_categories_from_dir(dir: &std::path::Path) -> anyhow::Result<CategoriesFile> {
    let macros_path = dir.join("macros.json");
    let macros_str = if macros_path.exists() {
        std::fs::read_to_string(&macros_path)?
    } else {
        "{}".to_owned()
    };

    let mut entries: Vec<_> = std::fs::read_dir(dir)?
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.path().extension().and_then(|s| s.to_str()) == Some("json")
                && e.file_name() != std::ffi::OsStr::new("macros.json")
        })
        .collect();
    entries.sort_by_key(|e| e.file_name());

    let mut categories_json = String::from("[");
    let mut first = true;
    for entry in entries {
        let stem = entry.path().file_stem().unwrap().to_string_lossy().to_string();
        let content = std::fs::read_to_string(entry.path())?;
        let mut obj: serde_json::Value = serde_json::from_str(&content)?;
        if let serde_json::Value::Object(ref mut map) = obj {
            map.insert("id".to_owned(), serde_json::Value::String(stem));
        }
        if !first { categories_json.push(','); }
        categories_json.push_str(&serde_json::to_string(&obj)?);
        first = false;
    }
    categories_json.push(']');

    let combined = format!("{{\"macros\":{macros_str},\"categories\":{categories_json}}}");
    Ok(serde_json::from_str(&combined)?)
}

/// Load shared, cross-topic macros from `topics/_shared/macros/<name>.json` (one Filter per
/// file, macro name = file stem). Referenced by name from any topic's conditions, e.g.
/// `{ "macro": "standard_exclude" }`. `shared_dir` is `topics/_shared`; only its `macros/`
/// subdirectory holds Filter macros — the data libraries (sanitizers.json, value_sets.json,
/// classifiers/) live at the `_shared/` root and are loaded explicitly elsewhere.
pub fn load_shared_macros(shared_dir: &std::path::Path) -> anyhow::Result<HashMap<String, Filter>> {
    let mut macros = HashMap::new();
    let dir = shared_dir.join("macros");
    if !dir.exists() {
        return Ok(macros);
    }
    for entry in std::fs::read_dir(&dir)? {
        let path = entry?.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let name = path.file_stem().unwrap().to_string_lossy().to_string();
        let filter: Filter = serde_json::from_str(&std::fs::read_to_string(&path)?)
            .with_context(|| format!("parsing shared macro {}", path.display()))?;
        macros.insert(name, filter);
    }
    Ok(macros)
}

// ── Filter evaluator ──────────────────────────────────────────────────────────

fn eval(filter: &Filter, ctx: &CategoryContext, macros: &HashMap<String, Filter>) -> bool {
    match filter {
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
            read_str(first_present(ctx, first_tag), sanitize, ctx.sanitizers)
                .is_some_and(|v| value_set(in_set).contains(v.as_ref())),
        Filter::FirstTagIn { first_tag, r#in, sanitize } =>
            read_str(first_present(ctx, first_tag), sanitize, ctx.sanitizers)
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

/// Read a numeric value for a `num` predicate. The reserved key `"length"` yields the context
/// length in metres; any other key reads that tag and, when `sanitize` is set, runs it through
/// that sanitizer chain (which may yield a JSON number, e.g. `parse_length`) before coercing to
/// f64. Returns None when the tag is absent or the value is unparseable — so every numeric
/// comparison is false on missing/garbage input.
fn read_num(ctx: &CategoryContext, key: &str, sanitize: &Option<String>) -> Option<f64> {
    if key == "length" {
        return Some(ctx.length_m);
    }
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

/// The value of the first key in `keys` that is present in the context tags.
fn first_present<'a>(ctx: &'a CategoryContext<'a>, keys: &[String]) -> Option<&'a str> {
    keys.iter().find_map(|k| ctx.tags.get(k)).map(String::as_str)
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

// ── Public API ────────────────────────────────────────────────────────────────

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
        length_m: 0.0,
        sanitizers,
    };
    eval(filter, &ctx, macros)
}

/// Find the first matching category for the given context using any CategoriesFile.
pub fn categorize<'a>(ctx: &CategoryContext, cats: &'a CategoriesFile) -> Option<&'a CategoryDef> {
    cats.categories.iter().find(|cat| {
        if !eval(&cat.condition, ctx, &cats.macros) {
            return false;
        }
        if let Some(excludes) = &cat.excludes {
            for excluded_id in excludes {
                // An exclude resolves to another category's condition, or — for conditions that
                // exist only to disqualify (e.g. `data_no`) — a macro of the same name.
                let cond = cats
                    .categories
                    .iter()
                    .find(|c| c.id == *excluded_id)
                    .map(|c| &c.condition)
                    .or_else(|| cats.macros.get(excluded_id));
                if let Some(cond) = cond {
                    if eval(cond, ctx, &cats.macros) {
                        return false;
                    }
                }
            }
        }
        true
    })
}

