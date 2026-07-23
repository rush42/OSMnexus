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
/// historical field names as canonical with `Filter`'s historical names (`tag`/`first_tag`) as
/// JSON aliases, so neither call site's existing configs needed to change. Untagged variants are
/// disambiguated by their own (distinct, required) field
/// name, so declaration order doesn't matter here; `sanitize` is optional and shared by all three,
/// so it never participates in that disambiguation. `Extract` is always `#[serde(flatten)]`ed into
/// its embedding struct, so `sanitize` living here rather than as a sibling field changes nothing
/// about the on-disk JSON shape.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Deserialize)]
#[serde(untagged)]
pub enum Extract {
    /// First-present fallback over an ordered candidate list.
    Candidates {
        #[serde(alias = "first_tag")]
        keys: Vec<String>,
        #[serde(default, deserialize_with = "crate::parser::deserialize_sanitize_chain")]
        sanitize: Vec<Sanitizer>,
    },
    /// A single, specific key.
    Value {
        #[serde(alias = "tag")]
        key: String,
        #[serde(default, deserialize_with = "crate::parser::deserialize_sanitize_chain")]
        sanitize: Vec<Sanitizer>,
    },
}

impl Extract {
    /// This variant's own `sanitize` chain — empty if unset.
    pub fn sanitize(&self) -> &[Sanitizer] {
        match self {
            Extract::Value { sanitize, .. } | Extract::Candidates { sanitize, .. } => sanitize,
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
    pub fn read(&self, ctx: &ExtractCtx) -> Option<Value> {
        eval_sanitize(self.sanitize(), self.read_raw(ctx)?)
    }

    /// Like `read`, coerced to a string — what every `Filter` `Tag*` comparison reads. A dropped
    /// sanitize output (or one that isn't string-shaped) compares as absent, not equal to "".
    pub fn read_str<'a>(&self, ctx: &ExtractCtx<'a>) -> Option<Cow<'a, str>> {
        let raw = self.read_raw(ctx)?;
        let chain = self.sanitize();
        if chain.is_empty() {
            return Some(Cow::Borrowed(raw));
        }
        match eval_sanitize(chain, raw)? {
            Value::String(s) => Some(Cow::Owned(s)),
            other => other.as_str().map(|s| Cow::Owned(s.to_owned())),
        }
    }

    /// The plain OSM key(s) this extract reads from — one for `Value`, the ordered candidate list
    /// for `Candidates`. Used by `categorize::linter`'s overlap grouping and
    /// `decision_tree`'s branch-key eligibility check (both only care about *which*
    /// tag(s) are read, not the `sanitize` chain).
    pub fn tag_names(&self) -> Vec<String> {
        match self {
            Extract::Value { key, .. } => vec![key.clone()],
            Extract::Candidates { keys, .. } => keys.clone(),
        }
    }

    /// This extract with every key prefixed (e.g. `"parent_"`), same `sanitize` chain — the
    /// `Filter::Parent` case: `categorize::linter::prefix_expr_tags` models a parent-scoped
    /// condition as an ordinary predicate on a synthetic `parent_<key>` name.
    pub fn prefixed(&self, prefix: &str) -> Extract {
        match self {
            Extract::Value { key, sanitize } => {
                Extract::Value { key: format!("{prefix}{key}"), sanitize: sanitize.clone() }
            }
            Extract::Candidates { keys, sanitize } => Extract::Candidates {
                keys: keys.iter().map(|k| format!("{prefix}{k}")).collect(),
                sanitize: sanitize.clone(),
            },
        }
    }

    /// Inverse of `prefixed` — strips `prefix` back off every key (a key without it is left as-is),
    /// same `sanitize` chain. `decision_tree`'s leaf-time evaluator uses this to recover
    /// the real tag name(s) before reading a parent-scoped predicate against `ExtractCtx::parent_tags`.
    pub fn strip_prefix(&self, prefix: &str) -> Extract {
        let strip = |k: &String| k.strip_prefix(prefix).map(str::to_owned).unwrap_or_else(|| k.clone());
        match self {
            Extract::Value { key, sanitize } => Extract::Value { key: strip(key), sanitize: sanitize.clone() },
            Extract::Candidates { keys, sanitize } => {
                Extract::Candidates { keys: keys.iter().map(strip).collect(), sanitize: sanitize.clone() }
            }
        }
    }
}

/// The first-present fallback over an ordered list of candidate keys — the single primitive
/// behind `Extract::Candidates`, also reused by
/// `categorize::transform::InputTransform::DirectedExtract`'s own key resolution. Returns the
/// first key that is set.
pub(crate) fn first_present<'a, K: AsRef<str>>(
    tags: &'a RawTags<'a>,
    keys: impl IntoIterator<Item = K>,
) -> Option<&'a str> {
    keys.into_iter().find_map(|k| tags.get(k.as_ref()).map(|v| v.as_ref()))
}
