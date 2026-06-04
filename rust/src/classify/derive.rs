//! Derivers: multi-input / context-dependent computations that produce a value from more
//! than one tag (and sometimes the matched category, side, or parent way). This is the
//! deliberate counterpart to `sanitize.rs`, whose functions are pure `&str -> atomic`.

use crate::osm::types::RawTags;
use crate::classify::sanitize::{get_sided, get_sided_with_bare_left, parse_length, SanitizerRegistry};

// ── parking inference (used by derive_traffic_mode) ─────────────────────────────

const PARKING_ALLOWED: &[&str] = &[
    "no", "yes", "lane", "street_side", "on_kerb", "half_on_kerb",
    "shoulder", "separate",
];

/// Port of SANITIZE_PARKING_TAGS.parking — returns the sanitized value or None.
fn sanitize_parking(raw: &str) -> Option<&str> {
    if PARKING_ALLOWED.contains(&raw) { Some(raw) } else { None }
}

/// Returns `Some("parking")` when `parking:{side}` (or `:both`) is a known
/// non-"no" value, mirroring Lua's `inferTrafficModeFromParking`.
fn infer_traffic_mode_from_parking(tags: &RawTags, side: &str) -> Option<String> {
    let raw = get_sided(tags, "parking", side)?;
    let v = sanitize_parking(raw)?;
    if v == "no" { return None; }
    Some("parking".to_owned())
}

/// Categories where parking inference applies to the transformed side only.
const DIRECTIONAL_PARKING_CATEGORIES: &[&str] = &[
    "cyclewayOnHighway_advisory",
    "cyclewayOnHighway_advisoryOrExclusive",
    "cyclewayOnHighway_exclusive",
    "cyclewayOnHighwayBetweenLanes",
    "cyclewayOnHighwayProtected",
];

fn is_bicycle_road(category_id: &str) -> bool {
    category_id == "bicycleRoad" || category_id == "bicycleRoad_vehicleDestination"
}

/// Single-output port of Lua's `deriveTrafficMode`, computing one side's value.
/// `out_side` ("left"|"right") selects which side this deriver emits; `obj_side` is the
/// transformed object's side (from context), used for the directional inference branch.
///
/// Note: single-*output* ≠ single-*input* — the explicit-tag gate is cross-side (an explicit
/// `traffic_mode:*` on *either* side suppresses inference on both), so both sides are read.
/// Equivalent to the former tuple `derive_traffic_mode`, projected onto `out_side`.
pub fn traffic_mode_side(
    obj_tags: &RawTags,       // transformed object tags
    centerline_tags: &RawTags, // parent way tags (for parking:*)
    category_id: &str,
    obj_side: &str,           // "left" | "right" | "self"
    out_side: &str,           // "left" | "right"
    reg: &SanitizerRegistry,
) -> Option<String> {
    // Sided, sanitized traffic_mode tag (normalize + allow-list via the data `traffic_mode`).
    let tm = |side: &str| {
        get_sided_with_bare_left(obj_tags, "traffic_mode", side)
            .and_then(|raw| apply_str(reg, "traffic_mode", raw))
    };

    // Explicit tags win (on either side) — no inference; emit this side's explicit value.
    let explicit_any = tm("left").is_some() || tm("right").is_some();
    if explicit_any {
        return tm(out_side);
    }

    // Bicycle roads: infer both sides from centerline parking tags.
    if is_bicycle_road(category_id) {
        return infer_traffic_mode_from_parking(centerline_tags, out_side);
    }

    // Lane categories: infer only the transformed side.
    if DIRECTIONAL_PARKING_CATEGORIES.contains(&category_id) && obj_side == out_side {
        return infer_traffic_mode_from_parking(centerline_tags, out_side);
    }

    None
}

// ── surface (with sett size split + parent copy) ────────────────────────────────────

fn apply_str(reg: &SanitizerRegistry, name: &str, raw: &str) -> Option<String> {
    reg.apply(name, raw).and_then(|v| v.as_str().map(str::to_owned))
}

/// The `sett` size split (Lua sanitize_tags.surface): refine `surface=sett` by `sett:length`.
fn sett_size(sett_length: Option<&str>) -> Option<&'static str> {
    let size = sett_length.and_then(parse_length)?;
    Some(if size <= 0.08 {
        "mosaic_sett"
    } else if size <= 0.13 {
        "small_sett"
    } else {
        "large_sett"
    })
}

/// Own-tags surface: the data-defined `surface` mapping, plus the multi-tag `sett` size split
/// (needs the sibling `sett:length`, hence Rust rather than a pure 1→1 sanitizer).
fn derive_surface_value(tags: &RawTags, reg: &SanitizerRegistry) -> Option<String> {
    let raw = tags.get("surface")?;
    if raw == "sett" {
        if let Some(sz) = sett_size(tags.get("sett:length").map(String::as_str)) {
            return Some(sz.to_owned());
        }
    }
    apply_str(reg, "surface", raw)
}

/// Own-tags surface deriver: data `surface` mapping + the multi-tag `sett` size split. The
/// parent copy (deriveBikelaneSurface) and provenance are orchestrated in `engine/extract.rs`.
pub fn surface(obj: &RawTags, reg: &SanitizerRegistry) -> Option<String> {
    derive_surface_value(obj, reg)
}

// ── derive_oneway ───────────────────────────────────────────────────────────────

/// Port of DeriveOneway.lua.
/// Returns one of: "yes" | "no" | "car_not_bike" | "assumed_no" | "implicit_yes"
///
/// `implicit_oneway` comes from the matched category's `implicit_oneway` field.
/// Returns the explicit/derived oneway, or `None` when there's no signal — in which case the
/// category's `oneway` const (the lowest-priority layer of `derived`) supplies the default
/// (`implicit_yes` for implicit categories, else `assumed_no`).
pub fn derive_oneway(tags: &RawTags) -> Option<String> {
    let oneway_bicycle = tags.get("oneway:bicycle").map(String::as_str);
    let oneway = tags.get("oneway").map(String::as_str);

    if oneway_bicycle == Some("yes") {
        return Some("yes".to_owned());
    }
    if oneway_bicycle == Some("no") {
        return Some(if oneway == Some("yes") { "car_not_bike" } else { "no" }.to_owned());
    }

    if matches!(oneway, Some("yes") | Some("no")) {
        return Some(oneway.unwrap().to_owned());
    }

    let highway = tags.get("highway").map(String::as_str);
    if matches!(highway, Some("service") | Some("track")) {
        return Some("assumed_no".to_owned());
    }

    None
}
