//! `Extract`: "resolve a raw tag value from a candidate key spec, then normalize it" — the one
//! piece of read logic `Filter`'s `Tag*` predicates and `Producer::Extract` both need, factored out
//! so it's written (and tested) once. Three key-resolution shapes: a single key, an ordered
//! candidate list (first-present wins), or a direction-sensitive key (`Directed`, resolved against
//! a full `ExtractCtx` rather than a bare tagset — see its own doc for why it needs more than
//! `first_present` can give it). `sanitize` lives on every variant, not as a field the
//! embedding type carries separately — every current embedding (`Filter`'s tag/num predicates,
//! `Producer::Extract`) pairs one `sanitize` with exactly one `Extract` 1:1, so there was no
//! independent axis of variation left to justify keeping them apart; `read`/`read_str` below no
//! longer take it as a parameter for that reason. Carries no `annotate`/provenance though — that's
//! a `Producer`-only concept (what a winning branch contributes), meaningless for a boolean
//! predicate, and not 1:1 with an `Extract` the way `sanitize` is (a `Const` or nested `Match` can
//! carry one too).

use std::borrow::Cow;

use serde::Deserialize;
use serde_json::Value;

use crate::lang::producer::ExtractCtx;
use crate::lang::sanitize::{resolve_sanitize, Sanitizer};
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
    /// Direction-sensitive read of `directed.key` — see `DirectedKey`'s own doc. Built from
    /// `transforms.json`'s `{ "directed": {...} }` sugar (see `parser`) — the
    /// object-cardinality-changing split itself stays native, but this per-key projection is an
    /// ordinary sided tag read.
    Directed {
        directed: DirectedKey,
        #[serde(default)]
        sanitize: Option<Sanitizer>,
    },
}

/// The `key`/`from` pair behind `Extract::Directed`: resolves `key`'s `:forward`/`:backward`
/// variant from `ctx.annotations["_side"]` + the global left/right-hand-traffic setting
/// (`traffic::is_left_hand_traffic`), producing nothing for a `self` object (no direction to
/// resolve). Not expressible as `Extract::Value`/`Candidates` plus a tagset wrapper: it needs both
/// tagsets at once — the object's own (to guard against overriding an already-set key) and, when
/// `from: Parent`, the parent's (tried bare-key-then-directed-key) — which is why `Extract::read*`
/// takes a full `ExtractCtx` rather than a bare `&RawTags`.
#[derive(Debug, Clone, Deserialize)]
pub struct DirectedKey {
    pub key: String,
    #[serde(default)]
    pub from: DirectedFrom,
}

/// Which tagset a directed read resolves against — its own, narrower vocabulary, distinct from
/// the general `TagSet` (`producer`): a directed read needs both `parent_tags` and the object's
/// own `obj_tags` simultaneously, so it can't be expressed as a plain "swap `obj_tags`, recurse"
/// wrapper the way every other tagset-scoping need is — see `DirectedKey`'s own doc. No
/// `ParentOrObj`: unlike the general case, a directed read has nothing distinct to commit to —
/// parent tags need the two-key (bare-then-directed) fallback `Producer::Parent` implements, so a
/// `ParentOrObj` here could only ever mean "try that, then plain `Obj`," which nobody's asked for;
/// keeping it out of the type means it can't be spelled by accident and silently behave like `Obj`.
#[derive(Debug, Deserialize, Clone, Copy, Default)]
#[serde(rename_all = "snake_case")]
pub enum DirectedFrom {
    #[default]
    Obj,
    Parent,
}

impl Extract {
    /// This variant's own `sanitize` chain, if any.
    pub fn sanitize(&self) -> Option<&Sanitizer> {
        match self {
            Extract::Value { sanitize, .. }
            | Extract::Candidates { sanitize, .. }
            | Extract::Directed { sanitize, .. } => sanitize.as_ref(),
        }
    }

    /// Resolve the raw string — a first-present fallback over the candidate list, a single key, or
    /// (`Directed`) a side/handedness-resolved key. Not yet run through `sanitize`.
    pub fn read_raw<'a>(&self, ctx: &ExtractCtx<'a>) -> Option<&'a str> {
        match self {
            Extract::Value { key, .. } => first_present(ctx.obj_tags, std::iter::once(key.as_str())),
            Extract::Candidates { keys, .. } => first_present(ctx.obj_tags, keys.iter().map(String::as_str)),
            Extract::Directed { directed, .. } => read_directed(directed, ctx),
        }
    }

    /// Read and run through `sanitize` (identity if unset) — what `Producer::Extract` produces.
    pub fn read(&self, ctx: &ExtractCtx) -> Option<Value> {
        resolve_sanitize(self.sanitize(), self.read_raw(ctx)?)
    }

    /// Like `read`, coerced to a string — what every `Filter` `Tag*` comparison reads. A dropped
    /// sanitize output (or one that isn't string-shaped) compares as absent, not equal to "".
    pub fn read_str<'a>(&self, ctx: &ExtractCtx<'a>) -> Option<Cow<'a, str>> {
        let raw = self.read_raw(ctx)?;
        match self.sanitize() {
            None => Some(Cow::Borrowed(raw)),
            Some(sanitize) => match resolve_sanitize(Some(sanitize), raw)? {
                Value::String(s) => Some(Cow::Owned(s)),
                other => other.as_str().map(|s| Cow::Owned(s.to_owned())),
            },
        }
    }
}

/// The read strategy behind `Extract::Directed` — see `DirectedKey`'s own doc for why it needs a
/// full `ExtractCtx`.
fn read_directed<'a>(directed: &DirectedKey, ctx: &ExtractCtx<'a>) -> Option<&'a str> {
    let DirectedKey { key, from } = directed;
    if ctx.obj_tags.contains_key(key) {
        return None; // already set (e.g. by an earlier unnest) — don't override it
    }
    let obj_side = ctx.annotations.get("_side").and_then(Value::as_str).unwrap_or("self");
    let suffix = match (obj_side, crate::traffic::is_left_hand_traffic()) {
        ("left", false) | ("right", true) => ":backward",
        ("right", false) | ("left", true) => ":forward",
        _ => return None, // "self": no direction to resolve
    };
    let directed_key = format!("{key}{suffix}");
    match from {
        DirectedFrom::Parent => {
            let tags = ctx.parent_tags?;
            first_present(tags, [key.as_str(), directed_key.as_str()])
        }
        DirectedFrom::Obj => first_present(ctx.obj_tags, [directed_key.as_str()]),
    }
}

/// The first-present fallback over an ordered list of candidate keys — the single primitive
/// behind `Extract::Candidates`. Returns the first key that is set.
fn first_present<K: AsRef<str>>(tags: &RawTags, keys: impl IntoIterator<Item = K>) -> Option<&str> {
    keys.into_iter().find_map(|k| tags.get(k.as_ref()).map(String::as_str))
}
