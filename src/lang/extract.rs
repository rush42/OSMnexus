//! `Extract`: "resolve a raw tag value from a candidate key spec, then normalize it" — the one
//! piece of read logic `Filter`'s `Tag*` predicates and `Producer::Extract` both need, factored out
//! so it's written (and tested) once. Two key-resolution shapes: a single key, or an ordered
//! candidate list (first-present wins) — a third, direction-sensitive shape used to live here
//! (`Directed`) but has moved to `categorize::transform::InputTransform::DirectedExtract`: every
//! real use of it was already spelled as a post-unnest step writing a plain output key (never a
//! live `Filter`/`Match` read), so its dual-tagset resolution now lives where it's actually needed
//! — `InputTransform::apply`, which already carries `parent_tags` — instead of being threaded
//! through every `Extract::read` call via a full `ExtractCtx`. `sanitize` lives on every variant,
//! not as a field the embedding type carries separately — every current embedding (`Filter`'s
//! tag/num predicates, `Producer::Extract`) pairs one `sanitize` with exactly one `Extract` 1:1, so
//! there was no independent axis of variation left to justify keeping them apart; `read`/`read_str`
//! below no longer take it as a parameter for that reason. Carries no `annotate`/provenance
//! though — that's a `Producer`-only concept (what a winning branch contributes), meaningless for a
//! boolean predicate, and not 1:1 with an `Extract` the way `sanitize` is (a `Const` or nested
//! `Match` can carry one too).

use std::borrow::Cow;

use serde::Deserialize;
use serde_json::Value;

use crate::lang::producer::ExtractCtx;
use crate::lang::sanitize::{eval_sanitize, Sanitizer};
use crate::osm::types::RawTags;

/// A candidate-key read spec plus its `sanitize` chain. `key`/`keys` accept `Producer`'s
/// historical field names as canonical with `Filter`'s historical names (`tag`/`first_tag`, and
/// `num` for its numeric predicates) as JSON aliases, so neither call site's existing configs
/// needed to change. Untagged variants are disambiguated by their own (distinct, required) field
/// name, so declaration order doesn't matter here; `sanitize` is optional and shared by all three,
/// so it never participates in that disambiguation. `Extract` is always `#[serde(flatten)]`ed into
/// its embedding struct, so `sanitize` living here rather than as a sibling field changes nothing
/// about the on-disk JSON shape.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum Extract {
    /// First-present fallback over an ordered candidate list.
    Candidates {
        #[serde(alias = "first_tag")]
        keys: Vec<String>,
        #[serde(default)]
        sanitize: Option<Sanitizer>,
    },
    /// A single, specific key.
    Value {
        #[serde(alias = "tag", alias = "num")]
        key: String,
        #[serde(default)]
        sanitize: Option<Sanitizer>,
    },
}

/// The outcome of resolving a value from tags: found (and, if `sanitize`d, accepted); the key(s)
/// simply weren't set; or a key *was* set and `sanitize` rejected it. `Absent` and `Rejected` look
/// the same to an `Option`-based caller, but a fallback chain (`Producer::Match`) must not treat
/// them alike: absence means "try the next branch," a rejection means the source had an opinion
/// and it was no — that should stop the whole search, not get silently papered over by whichever
/// unrelated branch happens to come next. See `producer::match_rules` for where the distinction is
/// spent.
#[derive(Debug, Clone)]
pub enum Presence<T> {
    Present(T),
    Absent,
    Rejected,
}

impl<T> Presence<T> {
    /// Collapse the distinction for callers that only care "did we get a value" (e.g. the
    /// outermost per-field emission in `topic::pipeline::eval_fields`, or `InputTransform`'s
    /// tag-rule application) — a rejection is still "no value" to them, it just isn't
    /// retry-eligible on the way there.
    pub fn into_option(self) -> Option<T> {
        match self {
            Presence::Present(v) => Some(v),
            Presence::Absent | Presence::Rejected => None,
        }
    }

    pub fn map<U>(self, f: impl FnOnce(T) -> U) -> Presence<U> {
        match self {
            Presence::Present(v) => Presence::Present(f(v)),
            Presence::Absent => Presence::Absent,
            Presence::Rejected => Presence::Rejected,
        }
    }
}

impl Extract {
    /// This variant's own `sanitize` chain, if any.
    pub fn sanitize(&self) -> Option<&Sanitizer> {
        match self {
            Extract::Value { sanitize, .. } | Extract::Candidates { sanitize, .. } => sanitize.as_ref(),
        }
    }

    /// Resolve the raw string — a first-present fallback over the candidate list, or a single key.
    /// Not yet run through `sanitize`.
    pub fn read_raw<'a>(&self, ctx: &ExtractCtx<'a>) -> Option<&'a str> {
        match self {
            Extract::Value { key, .. } => first_present(ctx.obj_tags, std::iter::once(key.as_str())),
            Extract::Candidates { keys, .. } => first_present(ctx.obj_tags, keys.iter().map(String::as_str)),
        }
    }

    /// Read and run through `sanitize` (identity if unset) — what `Producer::Extract` produces.
    /// Distinguishes "no candidate key was set" (`Absent`) from "a key was set but `sanitize`
    /// dropped its value" (`Rejected`) — see `Presence`'s own doc for why that split matters.
    pub fn read(&self, ctx: &ExtractCtx) -> Presence<Value> {
        let Some(raw) = self.read_raw(ctx) else { return Presence::Absent };
        match eval_sanitize(self.sanitize(), raw) {
            Some(value) => Presence::Present(value),
            None => Presence::Rejected,
        }
    }

    /// Like `read`, coerced to a string — what every `Filter` `Tag*` comparison reads. A dropped
    /// sanitize output (or one that isn't string-shaped) compares as absent, not equal to "".
    pub fn read_str<'a>(&self, ctx: &ExtractCtx<'a>) -> Option<Cow<'a, str>> {
        let raw = self.read_raw(ctx)?;
        match self.sanitize() {
            None => Some(Cow::Borrowed(raw)),
            Some(sanitize) => match eval_sanitize(Some(sanitize), raw)? {
                Value::String(s) => Some(Cow::Owned(s)),
                other => other.as_str().map(|s| Cow::Owned(s.to_owned())),
            },
        }
    }
}

/// The first-present fallback over an ordered list of candidate keys — the single primitive
/// behind `Extract::Candidates`, also reused by
/// `categorize::transform::InputTransform::DirectedExtract`'s own key resolution. Returns the
/// first key that is set.
pub(crate) fn first_present<K: AsRef<str>>(tags: &RawTags, keys: impl IntoIterator<Item = K>) -> Option<&str> {
    keys.into_iter().find_map(|k| tags.get(k.as_ref()).map(String::as_str))
}
