pub mod side_split;

use crate::osm::types::RawTags;

/// For every key starting with `prefix`, strip it, re-key the value onto the base tag, and stamp
/// a marker. The marker key is `<base>:<stamp_key>` when the base starts with one of
/// `stamp_nested_under`, else `stamp_key`. The one remaining native in-place tag transform: it
/// needs to iterate keys matching a runtime-unknown pattern, which no `Producer`/`Rule` primitive
/// can express (they all name their target key(s) statically). Everything else that used to live
/// here (`lifecycle`, `rename_key`, `value_cases`) is expressible as `tag_rules` `Producer`
/// entries and has moved to topic JSON.
pub fn strip_prefix(
    tags: &mut RawTags,
    prefix: &str,
    stamp_key: &str,
    stamp_value: &str,
    stamp_nested_under: &[String],
) {
    let matched: Vec<(String, String)> = tags
        .iter()
        .filter(|(k, _)| k.starts_with(prefix))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();

    for (key, value) in matched {
        let base = key[prefix.len()..].to_owned();
        tags.insert(base.clone(), value);
        tags.remove(&key);

        let marker = if stamp_nested_under.iter().any(|p| base.starts_with(p.as_str())) {
            format!("{base}:{stamp_key}")
        } else {
            stamp_key.to_owned()
        };
        tags.insert(marker, stamp_value.to_owned());
    }
}
