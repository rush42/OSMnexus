//! `Extract`: "read a raw tag value, optionally through a `sanitize` chain" — the one piece of read
//! logic `Filter`'s `Tag*` predicates and `Producer::Extract` both need, factored out so it's
//! written (and tested) once. A candidate-key spec (`key`, `keys`, or a `side`-expanded sided key)
//! resolved against a tagset via `keys::first_present`/`keys::sided_keys`, then run through an
//! optional `sanitize` chain (identity if unset — see `sanitize::resolve_sanitize`).
//!
//! Deliberately carries no `consts`/provenance — that's a `Producer`-only concept (what a winning
//! branch contributes), meaningless for a boolean predicate. `Producer::Extract` wraps one
//! alongside its own `consts`; `Filter`'s `Tag*` variants flatten one into themselves alongside
//! their comparison field (`eq`/`in`/`contains`/…).

use std::borrow::Cow;
use std::collections::HashMap;

use serde::Deserialize;
use serde_json::Value;

use crate::tag_engine::keys;
use crate::tag_engine::sanitize::{resolve_sanitize, Sanitizer, SanitizeRef};
use crate::osm::types::RawTags;

/// A candidate-key read spec plus its `sanitize` chain. `key`/`first_tag`/`side` accept `Producer`'s
/// historical field names as canonical (`key`/`keys`) with `Filter`'s historical names (`tag`/
/// `first_tag`) as JSON aliases, so neither call site's existing configs needed to change.
#[derive(Debug, Clone, Deserialize)]
pub struct Extract {
    #[serde(alias = "tag", default)]
    pub key: Option<String>,
    #[serde(alias = "first_tag", default)]
    pub keys: Option<Vec<String>>,
    /// Sided key expansion (`key:{side}` → `:both` → bare-left) — `Producer::Extract` only;
    /// `Filter`'s `Tag*` predicates never set this (they have their own, unrelated `Side` context
    /// predicate for `ctx.split.obj_side`).
    #[serde(default)]
    pub side: Option<String>,
    #[serde(default)]
    pub sanitize: Option<SanitizeRef>,
}

impl Extract {
    /// Resolve `sanitize:`'s named reference once, at load time (alongside `Filter::expand`/
    /// `Producer::resolve`) — `key`/`keys`/`side` carry no reference to resolve.
    pub fn resolve(&self, sanitizers: &HashMap<String, Sanitizer>) -> anyhow::Result<Extract> {
        Ok(Extract {
            key: self.key.clone(),
            keys: self.keys.clone(),
            side: self.side.clone(),
            sanitize: self.sanitize.as_ref().map(|r| r.resolve(sanitizers)).transpose()?,
        })
    }

    /// Resolve the raw string, ignoring `sanitize` — all three key forms are a first-present
    /// fallback over a candidate key list: a sided expansion, a single `key`, or the explicit
    /// `keys` list.
    pub fn read_raw<'a>(&self, tags: &'a RawTags) -> Option<&'a str> {
        if let Some(side) = &self.side {
            let candidates = keys::sided_keys(self.key.as_deref().expect("sided extract needs `key`"), side, true);
            return keys::first_present(tags, candidates);
        }
        if let Some(key) = &self.key {
            return keys::first_present(tags, std::iter::once(key.as_str()));
        }
        if let Some(keys) = &self.keys {
            return keys::first_present(tags, keys.iter().map(String::as_str));
        }
        None
    }

    /// Read and run through `sanitize` (identity if unset) — what `Producer::Extract` produces.
    pub fn read(&self, tags: &RawTags) -> Option<Value> {
        resolve_sanitize(self.sanitize.as_ref(), self.read_raw(tags)?)
    }

    /// Like `read`, coerced to a string — what every `Filter` `Tag*` comparison reads. A dropped
    /// sanitize output (or one that isn't string-shaped) compares as absent, not equal to "".
    pub fn read_str<'a>(&self, tags: &'a RawTags) -> Option<Cow<'a, str>> {
        let raw = self.read_raw(tags)?;
        match &self.sanitize {
            None => Some(Cow::Borrowed(raw)),
            Some(_) => match resolve_sanitize(self.sanitize.as_ref(), raw)? {
                Value::String(s) => Some(Cow::Owned(s)),
                other => other.as_str().map(|s| Cow::Owned(s.to_owned())),
            },
        }
    }
}
