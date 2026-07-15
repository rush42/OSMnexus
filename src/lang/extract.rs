//! `Extract`: "resolve a raw tag value from a candidate key spec" — the one piece of read logic
//! `Filter`'s `Tag*` predicates and `Producer::Extract` both need, factored out so it's written
//! (and tested) once. Two shapes, nothing else: a single key, or an ordered candidate list
//! (first-present wins). Resolved against a tagset via `keys::first_present`.
//!
//! Deliberately carries no `sanitize` — that's a sibling field wherever `Extract` is embedded
//! (`Filter::Tag*`, `Producer::Extract`), not part of the read primitive itself, so it can be
//! resolved (`SanitizeRef::resolve`) and applied uniformly by the embedding type instead of being
//! duplicated inside `Extract`'s own `resolve`/`read`. `read`/`read_str` below take it as a
//! parameter for that reason. Also carries no `annotate`/provenance — that's a `Producer`-only
//! concept (what a winning branch contributes), meaningless for a boolean predicate.
//! `Producer::Extract` wraps one alongside its own `sanitize`/`annotate`; `Filter`'s `Tag*` variants
//! flatten one into themselves alongside their own `sanitize` and comparison field (`eq`/`in`/…).

use std::borrow::Cow;

use serde::Deserialize;
use serde_json::Value;

use crate::lang::keys;
use crate::lang::sanitize::{resolve_sanitize, Sanitizer};
use crate::osm::types::RawTags;

/// A candidate-key read spec. `key`/`keys` accept `Producer`'s historical field names as canonical
/// with `Filter`'s historical names (`tag`/`first_tag`) as JSON aliases, so neither call site's
/// existing configs needed to change. Untagged variants are disambiguated by their own (distinct,
/// required) field name, so declaration order doesn't matter here.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum Extract {
    /// First-present fallback over an ordered candidate list.
    Candidates {
        #[serde(alias = "first_tag")]
        keys: Vec<String>,
    },
    /// A single, specific key.
    Value {
        #[serde(alias = "tag")]
        key: String,
    },
}

impl Extract {
    /// Resolve the raw string — a first-present fallback over the candidate list, or a single key.
    pub fn read_raw<'a>(&self, tags: &'a RawTags) -> Option<&'a str> {
        match self {
            Extract::Value { key } => keys::first_present(tags, std::iter::once(key.as_str())),
            Extract::Candidates { keys } => keys::first_present(tags, keys.iter().map(String::as_str)),
        }
    }

    /// Read and run through `sanitize` (identity if unset) — what `Producer::Extract` produces.
    pub fn read(&self, sanitize: Option<&Sanitizer>, tags: &RawTags) -> Option<Value> {
        resolve_sanitize(sanitize, self.read_raw(tags)?)
    }

    /// Like `read`, coerced to a string — what every `Filter` `Tag*` comparison reads. A dropped
    /// sanitize output (or one that isn't string-shaped) compares as absent, not equal to "".
    pub fn read_str<'a>(&self, sanitize: Option<&Sanitizer>, tags: &'a RawTags) -> Option<Cow<'a, str>> {
        let raw = self.read_raw(tags)?;
        match sanitize {
            None => Some(Cow::Borrowed(raw)),
            Some(_) => match resolve_sanitize(sanitize, raw)? {
                Value::String(s) => Some(Cow::Owned(s)),
                other => other.as_str().map(|s| Cow::Owned(s.to_owned())),
            },
        }
    }
}
