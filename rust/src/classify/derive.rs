//! Derivers: multi-input / context-dependent computations that produce a value from more
//! than one tag (and sometimes the matched category, side, or parent way). This is the
//! deliberate counterpart to `sanitize.rs`, whose functions are pure `&str -> atomic`.

use crate::osm::types::RawTags;
use crate::classify::sanitize::{get_sided, traffic_mode};

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

/// Port of Lua's `deriveTrafficMode` — returns `(traffic_mode_left, traffic_mode_right)`.
/// Uses explicit `traffic_mode:*` tags first; falls back to parking-lane inference.
pub fn derive_traffic_mode(
    bikelane_tags: &RawTags,  // transformed object tags
    centerline_tags: &RawTags, // parent way tags (for parking:*)
    category_id: &str,
    side: &str,               // "left" | "right" | "self"
) -> (Option<String>, Option<String>) {
    let tm_left  = traffic_mode(bikelane_tags, "left");
    let tm_right = traffic_mode(bikelane_tags, "right");

    // Explicit tags win — no inference needed.
    if tm_left.is_some() || tm_right.is_some() {
        return (tm_left, tm_right);
    }

    // Bicycle roads: infer both sides from centerline parking tags.
    if category_id == "bicycleRoad" || category_id == "bicycleRoad_vehicleDestination" {
        return (
            infer_traffic_mode_from_parking(centerline_tags, "left"),
            infer_traffic_mode_from_parking(centerline_tags, "right"),
        );
    }

    // Lane categories: infer only the transformed side.
    if DIRECTIONAL_PARKING_CATEGORIES.contains(&category_id) {
        let inferred = infer_traffic_mode_from_parking(centerline_tags, side);
        return match side {
            "left"  => (inferred, None),
            "right" => (None, inferred),
            _       => (None, None),
        };
    }

    (None, None)
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
