use std::collections::HashMap;

use anyhow::Context;
use serde::Deserialize;

use crate::engine::topic::DeriverBinding;
use crate::osm::types::RawTags;
use crate::output::types::Side;
use crate::classify::highway_classes::allowed_highways;
use crate::classify::sanitize::{normalize_separation, normalize_traffic_mode, SEPARATION_ALLOWED};

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
}

// ── Data types ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
pub struct CategoryDef {
    pub id: String,
    pub infrastructure_exists: bool,
    pub implicit_oneway: bool,
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
    LengthLte { length_lte: f64   },
    LengthLt  { length_lt:  f64   },
    HasKeyPrefix { has_key_prefix: String },
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

/// Load shared, cross-topic macros from `topics/_shared/<name>.json` (one Filter per file,
/// macro name = file stem). Referenced by name from any topic's conditions, e.g.
/// `{ "macro": "standard_exclude" }`.
pub fn load_shared_macros(dir: &std::path::Path) -> anyhow::Result<HashMap<String, Filter>> {
    let mut macros = HashMap::new();
    if !dir.exists() {
        return Ok(macros);
    }
    for entry in std::fs::read_dir(dir)? {
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
            "is_crossing_pattern"                       => is_crossing_pattern(ctx),
            "is_sidepath"                               => is_sidepath(ctx),
            "has_between_lanes_conditions"              => has_between_lanes_conditions(ctx),
            "is_footway_bicycle_yes_base"               => is_footway_bicycle_yes_base(ctx),
            "is_advisory_or_exclusive"                  => is_advisory_or_exclusive(ctx),
            "is_foot_and_cycleway_segregated_edge_case" => is_foot_and_cycleway_segregated_edge_case(ctx),
            "is_protected_bikelane_separation"          => is_protected_bikelane_separation(ctx),
            "is_allowed_highway"                        => is_allowed_highway(ctx),
            // JSON-defined macros (per-topic categories/macros.json + shared topics/_shared/)
            other => macros
                .get(other)
                .map(|f| eval(f, ctx, macros))
                .unwrap_or_else(|| { tracing::warn!("unknown macro: {}", other); false }),
        },

        Filter::TagEq { tag, eq } =>
            ctx.tags.get(tag).map(|v| v == eq).unwrap_or(false),
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
        Filter::LengthLte { length_lte } => ctx.length_m <= *length_lte,
        Filter::LengthLt  { length_lt  } => ctx.length_m <  *length_lt,
        Filter::HasKeyPrefix { has_key_prefix } =>
            ctx.tags.keys().any(|k| k.starts_with(has_key_prefix.as_str())),
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

fn parent_tag<'a>(ctx: &'a CategoryContext<'a>, key: &str) -> Option<&'a str> {
    ctx.parent_tags.and_then(|t| t.get(key)).map(String::as_str)
}

fn sign_contains(sign: Option<&str>, needle: &str) -> bool {
    sign.map(|s| s.contains(needle)).unwrap_or(false)
}


/// True when the way's `highway` value is one we process at all.
/// Backs the shared `standard_exclude` macro (replaces the old `should_exclude`).
fn is_allowed_highway(ctx: &CategoryContext) -> bool {
    ctx.tags
        .get("highway")
        .map(|hw| allowed_highways().contains(hw.as_str()))
        .unwrap_or(false)
}

/// Port of `IsSidepath` from IsSidepath.lua.
fn is_sidepath(ctx: &CategoryContext) -> bool {
    if tag_is(ctx, "is_sidepath", "no") { return false; }
    tag_is(ctx, "is_sidepath", "yes")
        || ctx.parent_highway.is_some()
        || tag_is(ctx, "footway", "sidewalk")
        || tag_is(ctx, "path", "sidewalk")
        || tag_is(ctx, "path", "sidepath")
        || tag_is(ctx, "cycleway", "sidepath")
        || tag_is(ctx, "steps", "sidewalk")
}

/// Port of `is_crossing_pattern` from BikelaneCategories.lua.
fn is_crossing_pattern(ctx: &CategoryContext) -> bool {
    let hw = tag(ctx, "highway");
    let cycleway = tag(ctx, "cycleway");
    if hw == Some("cycleway") && cycleway == Some("lane") && tag_is(ctx, "lane", "crossing") {
        return true;
    }
    if hw == Some("cycleway") && matches!(cycleway, Some("crossing") | Some("traffic_island")) {
        return true;
    }
    if hw == Some("path")
        && matches!(tag(ctx, "path"), Some("crossing") | Some("traffic_island"))
        && tag_in(ctx, "bicycle", &["yes", "designated"])
    { return true; }
    if hw == Some("footway")
        && matches!(tag(ctx, "footway"), Some("crossing") | Some("traffic_island"))
        && tag_in(ctx, "bicycle", &["yes", "designated"])
    { return true; }
    false
}

/// Port of hasCyclewayOnHighwayBetweenLanesConditions from BikelaneCategories.lua.
fn has_between_lanes_conditions(ctx: &CategoryContext) -> bool {
    if ctx.side == Side::Self_ { return false; }
    if sign_contains(tag(ctx, "lanes"), "|lane|") { return true; }
    if sign_contains(parent_tag(ctx, "bicycle:lanes"), "|designated|") { return true; }
    false
}

/// Port of the footwayBicycleYes base condition — kept in Rust for mtb:scale numeric parsing.
fn is_footway_bicycle_yes_base(ctx: &CategoryContext) -> bool {
    if is_crossing_pattern(ctx) { return false; }
    let hw = tag(ctx, "highway").unwrap_or("");
    if !matches!(hw, "footway" | "path") { return false; }
    let has_bicycle_access = tag_is(ctx, "bicycle", "yes")
        || sign_contains(tag(ctx, "traffic_sign"), "1022-10");
    if !has_bicycle_access { return false; }
    if let Some(mtb) = tag(ctx, "mtb:scale") {
        let cleaned: String = mtb.chars().filter(|c| !matches!(c, '+' | '-' | ' ')).collect();
        match cleaned.parse::<f64>() {
            Ok(n) if n > 1.0 => return false,
            Err(_) => return false,
            _ => {}
        }
        if tag(ctx, "traffic_sign").is_none() && tag(ctx, "is_sidepath").is_none() {
            return false;
        }
    }
    true
}

/// Port of the `footAndCyclewaySegregated` edge case (traffic_mode:right=foot + separation check).
/// Lua uses SANITIZE_ROAD_TAGS which normalises separation values; we replicate that here.
fn is_foot_and_cycleway_segregated_edge_case(ctx: &CategoryContext) -> bool {
    if !tag_is(ctx, "highway", "cycleway") { return false; }

    // traffic_mode:right (or :both) must be foot (Lua normalises foot;bicycle → foot)
    let tm_right = tag(ctx, "traffic_mode:right").or_else(|| tag(ctx, "traffic_mode:both"));
    let tm_foot = matches!(tm_right, Some("foot") | Some("foot;bicycle"));
    if !tm_foot { return false; }

    // separation:right (or :both) must not be a known blocking separation value.
    // Lua: Sanitize+transform returns nil for unknown values → treated as non-blocking.
    let sep_raw = tag(ctx, "separation:right").or_else(|| tag(ctx, "separation:both"));
    if let Some(raw) = sep_raw {
        let normalized = normalize_separation(raw);
        if SEPARATION_ALLOWED.contains(&normalized) && normalized != "no" {
            return false;
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

    let sep_left_raw = tag(ctx, "separation:left")
        .or_else(|| tag(ctx, "separation:both"))
        .or_else(|| tag(ctx, "separation"));
    let sep_left = sep_left_raw.map(normalize_separation);

    let sep_right_raw = tag(ctx, "separation:right")
        .or_else(|| tag(ctx, "separation:both"));
    let sep_right = sep_right_raw.map(normalize_separation);

    let tm_right_raw = tag(ctx, "traffic_mode:right")
        .or_else(|| tag(ctx, "traffic_mode:both"));
    let tm_right = tm_right_raw.map(normalize_traffic_mode);

    let tm_left_raw = tag(ctx, "traffic_mode:left")
        .or_else(|| tag(ctx, "traffic_mode:both"));
    let tm_left = tm_left_raw.map(normalize_traffic_mode);

    let has_segregated = tag(ctx, "segregated").is_some();

    // Case 1: physical separation left + NOT motor_vehicle right + no segregated
    if let Some(sl) = sep_left {
        if PHYSICAL.contains(&sl) {
            if tm_right != Some("motor_vehicle") && !has_segregated {
                return true;
            }
        }
    }

    // Case 2: parking left + no segregated
    if tm_left == Some("parking") && !has_segregated {
        return true;
    }

    // Case 3: counter-flow — motor_vehicle right + physical separation right
    if tm_right == Some("motor_vehicle") {
        if let Some(sr) = sep_right {
            if PHYSICAL.contains(&sr) {
                return true;
            }
        }
    }

    false
}

/// Port of cyclewayOnHighway_advisoryOrExclusive base — kept in Rust for lane-suffix interaction
/// with hasCyclewayOnHighwayBetweenLanesConditions.
fn is_advisory_or_exclusive(ctx: &CategoryContext) -> bool {
    if !tag_is(ctx, "highway", "cycleway") { return false; }
    if !tag_in(ctx, "cycleway", &["lane", "opposite_lane"]) { return false; }
    if has_between_lanes_conditions(ctx) {
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
pub fn eval_filter(filter: &Filter, tags: &RawTags, macros: &HashMap<String, Filter>) -> bool {
    let ctx = CategoryContext {
        tags,
        side: Side::Self_,
        prefix: None,
        parent_highway: None,
        parent_tags: None,
        infix: None,
        length_m: 0.0,
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

