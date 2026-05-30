use crate::osm::types::RawTags;

use super::highway_classes::allowed_highways;

/// Returns true if the way should be excluded from processing.
pub fn should_exclude(tags: &RawTags) -> bool {
    by_highway_class(tags)
        || by_other_tags(tags)
        || by_indoor(tags)
}

fn by_highway_class(tags: &RawTags) -> bool {
    match tags.get("highway").map(String::as_str) {
        None => true,
        Some(hw) => !allowed_highways().contains(hw),
    }
}

fn by_other_tags(tags: &RawTags) -> bool {
    tags.get("area").map(|v| v == "yes").unwrap_or(false)
        || tags.get("man_made").map(|v| v == "pier").unwrap_or(false)
        || tags.get("leisure").map(|v| v == "track").unwrap_or(false)
}

fn by_indoor(tags: &RawTags) -> bool {
    tags.get("indoor").map(|v| v == "yes").unwrap_or(false)
}

const FORBIDDEN_ACCESSES: &[&str] = &["private", "no", "delivery", "permit"];

/// Returns true if access restrictions prevent bikelane processing.
pub fn by_access_bikelanes(tags: &RawTags) -> bool {
    if let Some(access) = tags.get("access") {
        if FORBIDDEN_ACCESSES.contains(&access.as_str()) {
            return true;
        }
    }
    if tags.get("highway").map(|v| v == "footway").unwrap_or(false) {
        if let Some(foot) = tags.get("foot") {
            if FORBIDDEN_ACCESSES.contains(&foot.as_str()) {
                return true;
            }
        }
    }
    if tags.get("highway").map(|v| v == "cycleway").unwrap_or(false) {
        if let Some(bicycle) = tags.get("bicycle") {
            if FORBIDDEN_ACCESSES.contains(&bicycle.as_str()) {
                return true;
            }
        }
    }
    false
}

/// Returns true if a `highway=service` way should be excluded.
pub fn by_service(tags: &RawTags) -> bool {
    if tags.get("highway").map(|v| v == "service").unwrap_or(false) {
        // Explicit bicycle access overrides exclusion.
        if tags.get("bicycle").map(|v| v == "designated").unwrap_or(false) {
            return false;
        }
        for key in &["bicycle:left", "bicycle:right", "bicycle:both"] {
            if let Some(v) = tags.get(*key) {
                if v != "no" {
                    return false;
                }
            }
        }
        if let Some(service) = tags.get("service") {
            // Only "alley" is allowed; everything else (driveway, parking_aisle, etc.) is excluded.
            if service != "alley" {
                return true;
            }
        }
    }
    false
}
