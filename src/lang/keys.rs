//! Generic tag-key selection primitives, shared by `producer` (extraction) and `filter`
//! (`Extract::Candidates`) — nothing here is sanitizer-specific, so it doesn't belong bundled with
//! the sanitizer-chain engine.

use crate::osm::types::RawTags;

/// The first-present fallback over an ordered list of candidate keys — the single primitive
/// behind `Extract::Candidates`. Returns the first key that is set.
pub(crate) fn first_present<K: AsRef<str>>(
    tags: &RawTags,
    keys: impl IntoIterator<Item = K>,
) -> Option<&str> {
    keys.into_iter().find_map(|k| tags.get(k.as_ref()).map(String::as_str))
}
