use crate::osm::types::RawTags;

/// Port of transform_cycleway_both_postfix.lua.
///
/// `cycleway=no` → `cycleway:both=no` so the absence can be tracked per side.
pub fn transform_cycleway_both_postfix(tags: &mut RawTags) {
    if tags.get("cycleway").map(|v| v == "no").unwrap_or(false) {
        tags.remove("cycleway");
        tags.insert("cycleway:both".into(), "no".into());
    }
}
