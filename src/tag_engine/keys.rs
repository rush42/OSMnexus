//! Generic tag-key selection primitives, shared by `producer` (extraction), `filter`
//! (`first_tag`/sided predicates), and `derive` (the Rust derivers) — nothing here is
//! sanitizer-specific, so it doesn't belong bundled with the sanitizer-chain engine.

use crate::osm::types::RawTags;

/// The first-present fallback over an ordered list of candidate keys — the single primitive
/// behind both the `keys` extractor and the sided lookups. Returns the first key that is set.
pub(crate) fn first_present<K: AsRef<str>>(
    tags: &RawTags,
    keys: impl IntoIterator<Item = K>,
) -> Option<&str> {
    keys.into_iter().find_map(|k| tags.get(k.as_ref()).map(String::as_str))
}

/// Candidate keys for a sided read, as a fallback list: `key:{side}` → `key:both`
/// → (left only, when `bare_left`) the bare `key`. A sided lookup is just `first_present`
/// over this list — what `getSided` / `getSidedWithBareLeft` were in Lua.
pub(crate) fn sided_keys(key: &str, side: &str, bare_left: bool) -> Vec<String> {
    let mut keys = vec![format!("{key}:{side}"), format!("{key}:both")];
    if bare_left && side == "left" {
        keys.push(key.to_owned());
    }
    keys
}
