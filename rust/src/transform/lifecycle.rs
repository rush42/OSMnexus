use crate::classify::highway_classes::allowed_highways;
use crate::osm::types::RawTags;

/// Port of transform_lifecycle_tags.lua.
///
/// Mutates tags in-place:
/// - `highway=construction + construction=<allowed>` → swap highway, set lifecycle=construction
/// - access=no with construction/baustelle in notes → lifecycle=construction_no_access
/// - access=no with blocked/closure in notes → lifecycle=blocked
///
/// Returns the extracted lifecycle value (if any).
pub fn transform_lifecycle_tags(tags: &mut RawTags) -> Option<String> {
    if !tags.contains_key("highway") {
        return None;
    }

    let allowed = allowed_highways();

    // Handle highway=construction + construction=<valid highway>
    if let Some(construction_val) = tags.get("construction").cloned() {
        if allowed.contains(construction_val.as_str()) {
            tags.insert("highway".into(), construction_val);
            tags.remove("construction");
            tags.insert("lifecycle".into(), "construction".into());
            return Some("construction".into());
        }
    }

    // Check if access restrictions might be construction/baustelle or blocked/closed.
    let is_restricted = is_access_restricted(tags);

    if is_restricted {
        let combined = format!(
            "{} {} {}",
            tags.get("access:reason").map(String::as_str).unwrap_or(""),
            tags.get("description").map(String::as_str).unwrap_or(""),
            tags.get("note").map(String::as_str).unwrap_or(""),
        )
        .to_lowercase();

        if combined.contains("construction") || combined.contains("baustelle") {
            remove_access_tags(tags);
            tags.insert("lifecycle".into(), "construction_no_access".into());
            return Some("construction_no_access".into());
        }

        const BLOCKED_TERMS: &[&str] =
            &["sperrung", "gesperrt", "blockiert", "blocked", "closure", "closed"];

        let note_desc = format!(
            "{} {}",
            tags.get("description").map(String::as_str).unwrap_or(""),
            tags.get("note").map(String::as_str).unwrap_or(""),
        )
        .to_lowercase();

        if BLOCKED_TERMS.iter().any(|t| note_desc.contains(t)) {
            remove_access_tags(tags);
            tags.insert("lifecycle".into(), "blocked".into());
            return Some("blocked".into());
        }
    }

    None
}

fn is_access_restricted(tags: &RawTags) -> bool {
    if tags.get("access").map(|v| v == "no").unwrap_or(false) {
        return true;
    }
    if tags.get("highway").map(|v| v == "cycleway").unwrap_or(false)
        && tags.get("bicycle").map(|v| v == "no").unwrap_or(false)
    {
        return true;
    }
    if tags.get("highway").map(|v| v == "footway").unwrap_or(false)
        && tags.get("foot").map(|v| v == "no").unwrap_or(false)
    {
        return true;
    }
    false
}

fn remove_access_tags(tags: &mut RawTags) {
    tags.remove("access");
    if tags.get("highway").map(|v| v == "cycleway").unwrap_or(false) {
        tags.remove("bicycle");
    }
    if tags.get("highway").map(|v| v == "footway").unwrap_or(false) {
        tags.remove("foot");
    }
}
