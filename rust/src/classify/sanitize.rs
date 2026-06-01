/// Port of Lua's sanitize_road_tags.lua, SanitizeTrafficSign.lua, and DeriveOneway.lua.
/// All functions are pure (no side effects) and operate on raw tag maps.

use crate::osm::types::RawTags;
use crate::classify::bikelane_categories::CategoryDef;

// ── Helpers ───────────────────────────────────────────────────────────────────

fn get_sided<'a>(tags: &'a RawTags, key: &str, side: &str) -> Option<&'a str> {
    tags.get(&format!("{key}:{side}"))
        .or_else(|| tags.get(&format!("{key}:both")))
        .map(String::as_str)
}

fn get_sided_with_bare_left<'a>(tags: &'a RawTags, key: &str, side: &str) -> Option<&'a str> {
    let sided = get_sided(tags, key, side);
    if side == "left" {
        sided.or_else(|| tags.get(key).map(String::as_str))
    } else {
        sided
    }
}

fn in_list(value: &str, list: &[&str]) -> bool {
    list.contains(&value)
}

// ── parse_length ──────────────────────────────────────────────────────────────

/// Port of parse_length.lua. Converts OSM length strings to metres as f32.
/// Handles: "2.5", "2.5 m", "250 cm", "2500 mm", "8 ft", "8'6\"", …
pub fn parse_length(raw: &str) -> Option<f32> {
    let s = raw.trim();
    if s.is_empty() { return None; }

    // feet/inches: 8'6" or 8'
    if s.contains('\'') {
        let parts: Vec<&str> = s.split('\'').collect();
        let feet: f32 = parts[0].trim().parse().ok()?;
        let inches: f32 = parts.get(1)
            .map(|p| p.trim().trim_end_matches('"'))
            .and_then(|p| if p.is_empty() { Some("0") } else { Some(p) })
            .and_then(|p| p.parse().ok())
            .unwrap_or(0.0);
        return Some((feet * 12.0 + inches) * 0.0254);
    }

    // strip unit suffix and scale
    let (num_str, scale) = if let Some(n) = s.strip_suffix("km") {
        (n.trim(), 1000.0_f32)
    } else if let Some(n) = s.strip_suffix("cm") {
        (n.trim(), 0.01_f32)
    } else if let Some(n) = s.strip_suffix("mm") {
        (n.trim(), 0.001_f32)
    } else if let Some(n) = s.strip_suffix("ft") {
        (n.trim(), 0.3048_f32)
    } else if let Some(n) = s.strip_suffix(" m") {
        (n.trim(), 1.0_f32)
    } else if let Some(n) = s.strip_suffix('m') {
        (n.trim(), 1.0_f32)
    } else {
        (s, 1.0_f32)
    };

    let v: f32 = num_str.replace(',', ".").parse().ok()?;
    Some(v * scale)
}

// ── surface_color ─────────────────────────────────────────────────────────────

/// Port of SANITIZE_ROAD_TAGS.surface_color.
pub fn surface_color(tags: &RawTags) -> Option<String> {
    let raw = tags.get("surface:colour").or_else(|| tags.get("surface:color"))?;
    let mut v = raw.as_str();

    let transformed = match v {
        "none" | "grey" | "gray" | "silver" | "dimgray" | "#888888" => "no",
        "#b5565a" | "orange" => "red",
        "green;red" => "red;green",
        _ => v,
    };
    v = transformed;

    if in_list(v, &["red", "green", "red;green", "no"]) {
        Some(v.to_owned())
    } else {
        None
    }
}

// ── separation ────────────────────────────────────────────────────────────────

pub(crate) fn normalize_separation(raw: &str) -> &str {
    match raw {
        "separation_kerb" | "lane_separator" => "bump",
        "surface" => "no",
        "tree_row;kerb" | "kerb;tree_row" | "tree_row;kerb;parking_lane"
        | "grass_verge;tree_row" => "tree_row",
        "kerb;greenery" => "kerb",
        "parking_lane;kerb" | "solid_line;parking_lane" => "parking_lane",
        other => other,
    }
}

const SEPARATION_ALLOWED: &[&str] = &[
    "no", "bollard", "flex_post", "vertical_panel", "studs", "bump", "planter", "kerb",
    "fence", "jersey_barrier", "guard_rail", "structure", "ditch", "greenery", "hedge",
    "tree_row", "cone", "kerb;parking_lane", "kerb;bollard", "yes",
];

/// Port of SANITIZE_ROAD_TAGS.separation.
pub fn separation(tags: &RawTags, side: &str) -> Option<String> {
    let raw = get_sided_with_bare_left(tags, "separation", side)?;
    let normalized = normalize_separation(raw);
    if in_list(normalized, SEPARATION_ALLOWED) {
        Some(normalized.to_owned())
    } else {
        None
    }
}

// ── marking ───────────────────────────────────────────────────────────────────

const MARKING_ALLOWED: &[&str] = &[
    "solid_line", "dashed_line", "double_solid_line", "barred_area", "pictogram", "surface",
];

/// Port of SANITIZE_ROAD_TAGS.marking.
pub fn marking(tags: &RawTags, side: &str) -> Option<String> {
    let raw = get_sided_with_bare_left(tags, "marking", side)?;
    if in_list(raw, MARKING_ALLOWED) {
        Some(raw.to_owned())
    } else {
        None
    }
}

// ── traffic_mode ──────────────────────────────────────────────────────────────

pub(crate) fn normalize_traffic_mode(raw: &str) -> &str {
    match raw {
        "foot;bicycle" => "foot",
        "motorized" => "motor_vehicle",
        "none" => "no",
        other => other,
    }
}

const TRAFFIC_MODE_ALLOWED: &[&str] = &[
    "no", "motor_vehicle", "parking", "psv", "bicycle", "foot",
];

/// Port of SANITIZE_ROAD_TAGS.traffic_mode.
pub fn traffic_mode(tags: &RawTags, side: &str) -> Option<String> {
    let raw = get_sided_with_bare_left(tags, "traffic_mode", side)?;
    let normalized = normalize_traffic_mode(raw);
    if in_list(normalized, TRAFFIC_MODE_ALLOWED) {
        Some(normalized.to_owned())
    } else {
        None
    }
}

// ── buffer ────────────────────────────────────────────────────────────────────

/// Port of SANITIZE_ROAD_TAGS.buffer.
pub fn buffer(tags: &RawTags, side: &str) -> Option<f32> {
    let raw = get_sided_with_bare_left(tags, "buffer", side)?;
    match raw {
        "no" | "none" => Some(0.0),
        other => parse_length(other),
    }
}

// ── temporary ─────────────────────────────────────────────────────────────────

/// Returns "temporary" if `temporary=yes`, otherwise None.
pub fn temporary(tags: &RawTags) -> Option<&'static str> {
    if tags.get("temporary").map(String::as_str) == Some("yes") {
        Some("temporary")
    } else {
        None
    }
}

// ── sanitize_traffic_sign ─────────────────────────────────────────────────────

/// Port of SanitizeTrafficSign.lua.
/// Normalises format irregularities like "DE: 244,1020-30" → "DE:244,1020-30".
pub fn sanitize_traffic_sign(raw: &str) -> Option<String> {
    if raw.is_empty() { return None; }
    if raw == "no" || raw == "none" { return Some("none".to_owned()); }

    // Strip whitespace after delimiters first
    let stripped = raw.replace(", ", ",").replace("; ", ";");
    let s = stripped.as_str();

    // Already correctly prefixed
    if s.starts_with("DE:") && s.len() > 3 && !s[3..].starts_with(' ') {
        return Some(stripped);
    }

    // Known substitutions (order matters — more specific first)
    let substitutions: &[(&str, &str)] = &[
        ("DE: ", "DE:"),
        ("DE.", "DE:"),
        ("D:",  "DE:"),
        ("D.",  "DE:"),
        ("de:", "DE:"),
        ("DE1", "DE:1"),
        ("DE2", "DE:2"),
    ];
    for (prefix, replacement) in substitutions {
        if let Some(rest) = s.strip_prefix(prefix) {
            return Some(format!("{replacement}{rest}"));
        }
    }

    // Bare numeric: "244" → "DE:244", "1020-30" → "DE:1020-30"
    if s.chars().next().map(|c| c.is_ascii_digit()).unwrap_or(false) {
        return Some(format!("DE:{s}"));
    }

    // Free text: return cleaned
    Some(stripped)
}

// ── derive_oneway ─────────────────────────────────────────────────────────────

/// Port of DeriveOneway.lua.
/// Returns one of: "yes" | "no" | "car_not_bike" | "assumed_no" | "implicit_yes"
pub fn derive_oneway(tags: &RawTags, category: &CategoryDef) -> String {
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

    if category.implicit_oneway {
        return "implicit_yes".to_owned();
    }

    "assumed_no".to_owned()
}

// ── sanitize_bridge_tunnel ────────────────────────────────────────────────────

/// Returns Some(true) only for "yes", None otherwise (port of Sanitize(v, {"yes"})).
pub fn sanitize_yes_flag(tags: &RawTags, key: &str) -> Option<bool> {
    if tags.get(key).map(String::as_str) == Some("yes") {
        Some(true)
    } else {
        None
    }
}
