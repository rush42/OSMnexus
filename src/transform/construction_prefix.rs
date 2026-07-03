use crate::osm::types::RawTags;

/// Port of transform_construction_prefix.lua.
///
/// Turns `construction:cycleway:left=lane` into `cycleway:left=lane` + `cycleway:left:lifecycle=construction`,
/// and similarly for other `construction:*` keys.
pub fn transform_construction_prefix(tags: &mut RawTags) {
    const PREFIX: &str = "construction:";

    let construction_keys: Vec<(String, String)> = tags
        .iter()
        .filter(|(k, _)| k.starts_with(PREFIX))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();

    for (key, value) in construction_keys {
        let base_tag = &key[PREFIX.len()..];

        tags.insert(base_tag.to_owned(), value);
        tags.remove(&key);

        let lifecycle_key = if base_tag.starts_with("cycleway:") || base_tag.starts_with("sidewalk:") {
            format!("{base_tag}:lifecycle")
        } else {
            "lifecycle".to_owned()
        };
        tags.insert(lifecycle_key, "construction".into());
    }
}
