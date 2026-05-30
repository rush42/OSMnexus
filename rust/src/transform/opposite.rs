use crate::osm::types::RawTags;

/// Port of transform_cycleway_opposite_schema.lua.
///
/// Normalises older OSM `cycleway=opposite*` tagging to the explicit left/right schema.
pub fn transform_cycleway_opposite_schema(tags: &mut RawTags) {
    match tags.get("cycleway").map(String::as_str) {
        Some("opposite") => {
            tags.remove("cycleway");
            tags.insert("oneway:bicycle".into(), "no".into());
        }
        Some("opposite_lane") => {
            tags.remove("cycleway");
            tags.insert("cycleway:left".into(), "lane".into());
            tags.insert("oneway:bicycle".into(), "no".into());
        }
        Some("opposite_track") => {
            tags.remove("cycleway");
            tags.insert("cycleway:left".into(), "track".into());
            tags.insert("oneway:bicycle".into(), "no".into());
        }
        _ => {}
    }
}
