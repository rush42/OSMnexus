//! Derivers: multi-input / context-dependent computations that produce a value from more
//! than one tag (and sometimes the matched category, side, or parent way). This is the
//! deliberate counterpart to `sanitize.rs`, whose functions are pure `&str -> atomic`.

use crate::osm::types::RawTags;
use crate::classify::sanitize::{get_sided, traffic_mode, SanitizerRegistry};

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
) -> Option<String> {
    // Explicit tags win (on either side) — no inference; emit this side's explicit value.
    let explicit_any =
        traffic_mode(obj_tags, "left").is_some() || traffic_mode(obj_tags, "right").is_some();
    if explicit_any {
        return traffic_mode(obj_tags, out_side);
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

// ── smoothness with parent copy ───────────────────────────────────────────────────

fn apply_str(reg: &SanitizerRegistry, name: &str, raw: &str) -> Option<String> {
    reg.apply(name, raw).and_then(|v| v.as_str().map(str::to_owned))
}

/// Own-tags smoothness (the 4-source derivation, mirroring the `smoothness` deriver in data).
/// Returns `(value, from_tag)` where `from_tag` marks the `smoothness` tag source (Lua's
/// "tag"/"tag_normalized" okSources) vs. a value derived from surface/tracktype/mtb:scale.
fn derive_smoothness(tags: &RawTags, reg: &SanitizerRegistry) -> (Option<String>, bool) {
    if let Some(raw) = tags.get("smoothness") {
        if let Some(v) = apply_str(reg, "smoothness_normalize", raw) {
            return (Some(v), true);
        }
    }
    for (key, san) in [
        ("surface", "surface_to_smoothness"),
        ("tracktype", "tracktype_to_smoothness"),
        ("mtb:scale", "mtb_scale_to_smoothness"),
    ] {
        if let Some(raw) = tags.get(key) {
            if let Some(v) = apply_str(reg, san, raw) {
                return (Some(v), false);
            }
        }
    }
    (None, false)
}

/// Port of `deriveBikelaneSmoothness`: own 4-source smoothness, then copy the parent highway's
/// smoothness under the Lua guards. Used by copy categories (`smoothness_from_parent`). For an
/// object with no parent (e.g. bicycleRoad self) this is just the own derivation.
pub fn smoothness_with_parent(
    obj: &RawTags,
    parent: Option<&RawTags>,
    reg: &SanitizerRegistry,
) -> Option<String> {
    let (own, own_from_tag) = derive_smoothness(obj, reg);
    let Some(parent) = parent else { return own };

    let (par, _) = derive_smoothness(parent, reg);
    if par.is_none() {
        return own;
    }

    let own_surface = obj.get("surface");
    let surfaces_match = own_surface == parent.get("surface");

    // A: own smoothness absent, and own surface absent or equal to the parent's.
    let cond_a = own.is_none() && (own_surface.is_none() || surfaces_match);
    // B: own smoothness not tag-sourced (derived or absent), own surface present and equal.
    let cond_b = !own_from_tag && own_surface.is_some() && surfaces_match;

    if cond_a || cond_b { par } else { own }
}

// ── derive_oneway ───────────────────────────────────────────────────────────────

/// Port of DeriveOneway.lua.
/// Returns one of: "yes" | "no" | "car_not_bike" | "assumed_no" | "implicit_yes"
///
/// `implicit_oneway` comes from the matched category's `implicit_oneway` field.
pub fn derive_oneway(tags: &RawTags, implicit_oneway: bool) -> String {
    let oneway_bicycle = tags.get("oneway:bicycle").map(String::as_str);
    let oneway = tags.get("oneway").map(String::as_str);

    if oneway_bicycle == Some("yes") {
        return "yes".to_owned();
    }
    if oneway_bicycle == Some("no") {
        return if oneway == Some("yes") {
            "car_not_bike".to_owned()
        } else {
            "no".to_owned()
        };
    }

    if matches!(oneway, Some("yes") | Some("no")) {
        return oneway.unwrap().to_owned();
    }

    let highway = tags.get("highway").map(String::as_str);
    if matches!(highway, Some("service") | Some("track")) {
        return "assumed_no".to_owned();
    }

    if implicit_oneway {
        return "implicit_yes".to_owned();
    }

    "assumed_no".to_owned()
}
