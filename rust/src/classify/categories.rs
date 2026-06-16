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
    pub infrastructure_exists: bool,
    pub implicit_oneway_confidence: String,
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

    // Tag predicates — secondary field disambiguates (TagEq is the catch-all)
    /// Membership in a named set from `_shared/value_sets.json` (keeps long value lists in data).
    TagInSet     { tag: String, in_set:      String      },
    TagIn        { tag: String, r#in:        Vec<String> },
    TagContains  { tag: String, contains:    String      },
    TagStartsWith{ tag: String, starts_with: String      },
    TagExists    { tag: String, exists:      bool        },
    TagEq        { tag: String, eq:          String      },

    // "First matching tag from a list" — tries each key in order, uses the first that exists
    FirstTagIn   { first_tag: Vec<String>, r#in:     Vec<String> },
    FirstTagExists { first_tag: Vec<String>, exists: bool        },

    // Parent tag predicates
    ParentTagIn        { parent_tag: String, r#in:        Vec<String> },
    ParentTagContains  { parent_tag: String, contains:    String      },
    ParentTagStartsWith{ parent_tag: String, starts_with: String      },
    ParentTagEq        { parent_tag: String, eq:          String      },

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
/// road_classification.json) live at the `_shared/` root and are loaded explicitly elsewhere.
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

        Filter::Macro { r#macro: name } => match name.as_str() {
            // Rust-implemented predicates — too complex or structural for JSON
            "is_advisory_or_exclusive"                  => is_advisory_or_exclusive(ctx, macros),
            "is_foot_and_cycleway_segregated_edge_case" => is_foot_and_cycleway_segregated_edge_case(ctx),
            "is_protected_bikelane_separation"          => is_protected_bikelane_separation(ctx),
            // JSON-defined macros (per-topic categories/macros.json + shared topics/_shared/)
            other => macros
                .get(other)
                .map(|f| eval(f, ctx, macros))
                .unwrap_or_else(|| { tracing::warn!("unknown macro: {}", other); false }),
        },

        Filter::TagEq { tag, eq } =>
            ctx.tags.get(tag).map(|v| v == eq).unwrap_or(false),
        Filter::TagInSet { tag, in_set } =>
            ctx.tags.get(tag).map(|v| value_set(in_set).contains(v)).unwrap_or(false),
        Filter::TagIn { tag, r#in } =>
            ctx.tags.get(tag).map(|v| r#in.iter().any(|s| s == v)).unwrap_or(false),
        Filter::TagContains { tag, contains } =>
            ctx.tags.get(tag).map(|v| v.contains(contains.as_str())).unwrap_or(false),
        Filter::TagStartsWith { tag, starts_with } =>
            ctx.tags.get(tag).map(|v| v.starts_with(starts_with.as_str())).unwrap_or(false),
        Filter::TagExists { tag, exists } =>
            ctx.tags.contains_key(tag) == *exists,

        Filter::FirstTagIn { first_tag, r#in } =>
            first_tag.iter()
                .find_map(|k| ctx.tags.get(k))
                .map(|v| r#in.iter().any(|s| s == v))
                .unwrap_or(false),
        Filter::FirstTagExists { first_tag, exists } =>
            first_tag.iter().any(|k| ctx.tags.contains_key(k)) == *exists,

        Filter::ParentTagEq { parent_tag, eq } =>
            ctx.parent_tags.and_then(|t| t.get(parent_tag)).map(|v| v == eq).unwrap_or(false),
        Filter::ParentTagIn { parent_tag, r#in } =>
            ctx.parent_tags.and_then(|t| t.get(parent_tag))
                .map(|v| r#in.iter().any(|s| s == v))
                .unwrap_or(false),
        Filter::ParentTagContains { parent_tag, contains } =>
            ctx.parent_tags.and_then(|t| t.get(parent_tag))
                .map(|v| v.contains(contains.as_str()))
                .unwrap_or(false),
        Filter::ParentTagStartsWith { parent_tag, starts_with } =>
            ctx.parent_tags.and_then(|t| t.get(parent_tag))
                .map(|v| v.starts_with(starts_with.as_str()))
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

// ── Rust-implemented predicates ───────────────────────────────────────────────
// These are structurally complex (multi-tag normalization, numeric parsing,
// cross-field interaction) and are unlikely to vary by jurisdiction.

fn tag<'a>(ctx: &'a CategoryContext<'a>, key: &str) -> Option<&'a str> {
    ctx.tags.get(key).map(String::as_str)
}

fn tag_is(ctx: &CategoryContext, key: &str, val: &str) -> bool {
    ctx.tags.get(key).map(|v| v == val).unwrap_or(false)
}

fn tag_in(ctx: &CategoryContext, key: &str, vals: &[&str]) -> bool {
    ctx.tags.get(key).map(|v| vals.contains(&v.as_str())).unwrap_or(false)
}

/// Run a data sanitizer (e.g. "separation", "traffic_mode") over a raw value, returning the
/// cleaned string (or None if dropped). Lets predicates share the sanitizers.json tables.
fn sanitize_str(ctx: &CategoryContext, name: &str, raw: &str) -> Option<String> {
    ctx.sanitizers.apply(name, raw).and_then(|v| v.as_str().map(str::to_owned))
}

fn parent_tag<'a>(ctx: &'a CategoryContext<'a>, key: &str) -> Option<&'a str> {
    ctx.parent_tags.and_then(|t| t.get(key)).map(String::as_str)
}

/// Evaluate a JSON-defined macro by name. Used by the remaining Rust predicates that still
/// depend on a now-datafied sub-condition (`has_between_lanes_conditions`).
/// Missing macro → false, matching the dispatch fallback.
fn eval_macro(name: &str, ctx: &CategoryContext, macros: &HashMap<String, Filter>) -> bool {
    macros.get(name).map(|f| eval(f, ctx, macros)).unwrap_or(false)
}

/// Port of the `footAndCyclewaySegregated` edge case (traffic_mode:right=foot + separation check).
/// Lua uses SANITIZE_ROAD_TAGS which normalises separation values; we replicate that here.
fn is_foot_and_cycleway_segregated_edge_case(ctx: &CategoryContext) -> bool {
    if !tag_is(ctx, "highway", "cycleway") { return false; }

    // traffic_mode:right (or :both) must be foot (the `traffic_mode` sanitizer folds foot;bicycle→foot)
    let tm_right = tag(ctx, "traffic_mode:right").or_else(|| tag(ctx, "traffic_mode:both"));
    let tm_foot = tm_right.and_then(|r| sanitize_str(ctx, "traffic_mode", r)).as_deref() == Some("foot");
    if !tm_foot { return false; }

    // separation:right (or :both) must not be a known blocking separation value.
    // A sanitized value is in the allow-list; treat any allowed non-"no" value as blocking.
    let sep_raw = tag(ctx, "separation:right").or_else(|| tag(ctx, "separation:both"));
    if let Some(raw) = sep_raw {
        if let Some(sep) = sanitize_str(ctx, "separation", raw) {
            if sep != "no" {
                return false;
            }
        }
    }

    true
}

/// Port of the cyclewayOnHighwayProtected separation check using Lua's SANITIZE_ROAD_TAGS.
/// Returns true when the context matches a "protected bikelane" separation condition.
fn is_protected_bikelane_separation(ctx: &CategoryContext) -> bool {
    const PHYSICAL: &[&str] = &[
        "bollard", "flex_post", "vertical_panel", "studs", "bump", "planter",
        "fence", "jersey_barrier", "guard_rail",
    ];

    let sided = |key_main: &str, key_both: &str, bare: Option<&str>, san: &str| -> Option<String> {
        tag(ctx, key_main)
            .or_else(|| tag(ctx, key_both))
            .or_else(|| bare.and_then(|b| tag(ctx, b)))
            .and_then(|raw| sanitize_str(ctx, san, raw))
    };

    let sep_left = sided("separation:left", "separation:both", Some("separation"), "separation");
    let sep_right = sided("separation:right", "separation:both", None, "separation");
    let tm_right = sided("traffic_mode:right", "traffic_mode:both", None, "traffic_mode");
    let tm_left = sided("traffic_mode:left", "traffic_mode:both", None, "traffic_mode");

    let has_segregated = tag(ctx, "segregated").is_some();
    let is_physical = |s: &Option<String>| s.as_deref().map_or(false, |v| PHYSICAL.contains(&v));

    // Case 1: physical separation left + NOT motor_vehicle right + no segregated
    if is_physical(&sep_left) && tm_right.as_deref() != Some("motor_vehicle") && !has_segregated {
        return true;
    }

    // Case 2: parking left + no segregated
    if tm_left.as_deref() == Some("parking") && !has_segregated {
        return true;
    }

    // Case 3: counter-flow — motor_vehicle right + physical separation right
    if tm_right.as_deref() == Some("motor_vehicle") && is_physical(&sep_right) {
        return true;
    }

    false
}

/// Port of cyclewayOnHighway_advisoryOrExclusive base — kept in Rust for lane-suffix interaction
/// with hasCyclewayOnHighwayBetweenLanesConditions.
fn is_advisory_or_exclusive(ctx: &CategoryContext, macros: &HashMap<String, Filter>) -> bool {
    if !tag_is(ctx, "highway", "cycleway") { return false; }
    if !tag_in(ctx, "cycleway", &["lane", "opposite_lane"]) { return false; }
    if eval_macro("has_between_lanes_conditions", ctx, macros) {
        let lanes = tag(ctx, "lanes").unwrap_or("");
        let bicycle_lanes = parent_tag(ctx, "bicycle:lanes").unwrap_or("");
        if lanes.contains("|lane|") && !lanes.ends_with("|lane") { return false; }
        if bicycle_lanes.contains("|designated|") && !bicycle_lanes.ends_with("|designated") { return false; }
    }
    true
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
                if let Some(excluded_cat) = cats.categories.iter().find(|c| c.id == *excluded_id) {
                    if eval(&excluded_cat.condition, ctx, &cats.macros) {
                        return false;
                    }
                }
            }
        }
        true
    })
}

