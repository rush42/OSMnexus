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
use crate::lang::tags::RawTags;

/// A candidate-key read spec plus its `sanitize` chain. `key`/`keys` accept `Producer`'s
/// historical field names as canonical with `Filter`'s historical names (`tag`/`first_tag`) as
/// JSON aliases, so neither call site's existing configs needed to change. Untagged variants are
/// disambiguated by their own (distinct, required) field
/// name, so declaration order doesn't matter here; `sanitize` is optional and shared by all three,
/// so it never participates in that disambiguation. `Extract` is always `#[serde(flatten)]`ed into
/// its embedding struct, so `sanitize` living here rather than as a sibling field changes nothing
/// about the on-disk JSON shape.
///
/// A key starting with `@` is a **virtual tag** ([`VirtualKey`]): a fact about the element rather
/// than one of its OSM tags, readable everywhere a tag is (filters, producers, `parent`, `in`).
/// It is resolved to [`Extract::Virtual`] when the config loads, so no element ever pays a string
/// check for it, and an unknown `@` key is a load error rather than a silently absent value.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Deserialize)]
#[serde(try_from = "ExtractJson")]
pub enum Extract {
    /// First-present fallback over an ordered candidate list.
    Candidates { keys: Vec<String>, sanitize: Vec<Sanitizer> },
    /// A single, specific key.
    Value { key: String, sanitize: Vec<Sanitizer> },
    /// A virtual tag (`@id`, `@side`, …). `parent` marks the parent-scoped form — the virtual-tag
    /// counterpart of a `parent_<key>` name (see `prefixed`).
    Virtual { field: VirtualKey, parent: bool, sanitize: Vec<Sanitizer> },
}

/// The on-disk shapes, before `@` keys are resolved. `key`/`keys` accept `Producer`'s historical
/// field names as canonical with `Filter`'s historical names (`tag`/`first_tag`) as aliases.
#[derive(Deserialize)]
#[serde(untagged)]
enum ExtractJson {
    Candidates {
        #[serde(alias = "first_tag")]
        keys: Vec<String>,
        #[serde(default, deserialize_with = "crate::lang::json::deserialize_sanitize_chain")]
        sanitize: Vec<Sanitizer>,
    },
    Value {
        #[serde(alias = "tag")]
        key: String,
        #[serde(default, deserialize_with = "crate::lang::json::deserialize_sanitize_chain")]
        sanitize: Vec<Sanitizer>,
    },
}

impl TryFrom<ExtractJson> for Extract {
    type Error = String;

    fn try_from(raw: ExtractJson) -> Result<Self, String> {
        match raw {
            ExtractJson::Value { key, sanitize } => Extract::key(key, sanitize),
            ExtractJson::Candidates { keys, sanitize } => Extract::keys(keys, sanitize),
        }
    }
}

impl Extract {
    /// A single-key read, resolving an `@` key to its virtual tag. Every place that builds an
    /// `Extract` from a config-supplied key goes through this or [`Extract::keys`], so `@` keys mean
    /// the same thing in every JSON shorthand.
    pub fn key(key: String, sanitize: Vec<Sanitizer>) -> Result<Extract, String> {
        match key.strip_prefix('@') {
            Some(name) => Ok(Extract::Virtual { field: VirtualKey::parse(name)?, parent: false, sanitize }),
            None => Ok(Extract::Value { key, sanitize }),
        }
    }

    /// A first-present read over candidate keys. A virtual tag can't be one of several candidates
    /// (it's not a tag the element might or might not carry), except as the only one.
    pub fn keys(mut keys: Vec<String>, sanitize: Vec<Sanitizer>) -> Result<Extract, String> {
        if keys.len() == 1 && keys[0].starts_with('@') {
            return Extract::key(keys.remove(0), sanitize);
        }
        if let Some(k) = keys.iter().find(|k| k.starts_with('@')) {
            return Err(format!("virtual tag {k:?} can't be one of several candidate keys; read it on its own"));
        }
        Ok(Extract::Candidates { keys, sanitize })
    }
}

/// The virtual tags: facts about the element being classified, spelled as `@`-prefixed keys
/// (the convention osmium uses for the same purpose) so they read with the ordinary tag syntax.
/// They are not tags: `has_key_prefix`, `tags_empty` and tag passthrough never see them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum VirtualKey {
    /// `@id` — the element's OSM id. Read through `parent`, a member way's parent relation's id.
    Id,
    /// `@type` — `node` / `way` / `relation`.
    Type,
    /// `@side` — a side-split object's side; `self` for every base object (and for a parent).
    Side,
    /// `@prefix` / `@infix` — the side-split transform's recorded prefix/infix; absent otherwise.
    Prefix,
    Infix,
}

impl VirtualKey {
    pub const ALL: [VirtualKey; 5] = [VirtualKey::Id, VirtualKey::Type, VirtualKey::Side, VirtualKey::Prefix, VirtualKey::Infix];

    pub fn name(self) -> &'static str {
        match self {
            VirtualKey::Id => "@id",
            VirtualKey::Type => "@type",
            VirtualKey::Side => "@side",
            VirtualKey::Prefix => "@prefix",
            VirtualKey::Infix => "@infix",
        }
    }

    fn parse(name: &str) -> Result<Self, String> {
        Self::ALL.into_iter().find(|v| &v.name()[1..] == name).ok_or_else(|| {
            let known: Vec<&str> = Self::ALL.iter().map(|v| v.name()).collect();
            format!("unknown virtual tag \"@{name}\" (known: {})", known.join(", "))
        })
    }

    fn read<'a>(self, ctx: &ExtractCtx<'a>) -> Option<Cow<'a, str>> {
        match self {
            VirtualKey::Id => ctx.elem.map(|e| Cow::Owned(e.osm_id.to_string())),
            VirtualKey::Type => ctx.elem.map(|e| Cow::Borrowed(e.kind.id_prefix())),
            VirtualKey::Side => Some(Cow::Borrowed(crate::lang::producer::side_of(ctx))),
            VirtualKey::Prefix => ctx.annotations.get("_prefix").and_then(Value::as_str).map(Cow::Borrowed),
            VirtualKey::Infix => ctx.annotations.get("_infix").and_then(Value::as_str).map(Cow::Borrowed),
        }
    }
}

impl Extract {
    /// This variant's own `sanitize` chain — empty if unset.
    pub fn sanitize(&self) -> &[Sanitizer] {
        match self {
            Extract::Value { sanitize, .. } | Extract::Candidates { sanitize, .. } | Extract::Virtual { sanitize, .. } => sanitize,
        }
    }

    /// Resolve the raw string — a first-present fallback over the candidate list, a single key, or
    /// a virtual tag. Not yet run through `sanitize`. Borrowed for every tag read; only `@id` formats
    /// a new string, and only when it is actually read.
    ///
    /// A parent-scoped `Virtual` read here (not via `decision_tree`'s parent redirect) reads the
    /// parent: a `Filter`/`Producer` `Parent` wrapper has already swapped `ctx` to
    /// `ExtractCtx::parent_scope`, and the flag only exists for the decision tree's flattened form.
    pub fn read_raw<'a>(&self, ctx: &ExtractCtx<'a>) -> Option<Cow<'a, str>> {
        match self {
            Extract::Value { key, .. } => first_present(ctx.obj_tags, std::iter::once(key.as_str())).map(Cow::Borrowed),
            Extract::Candidates { keys, .. } => first_present(ctx.obj_tags, keys.iter().map(String::as_str)).map(Cow::Borrowed),
            Extract::Virtual { field, .. } => field.read(ctx),
        }
    }

    /// Read and run through `sanitize` (identity if unset) — what `Producer::Extract` produces.
    pub fn read(&self, ctx: &ExtractCtx) -> Option<Value> {
        eval_sanitize(self.sanitize(), &self.read_raw(ctx)?)
    }

    /// Like `read`, coerced to a string — what every `Filter` `Tag*` comparison reads. A dropped
    /// sanitize output (or one that isn't string-shaped) compares as absent, not equal to "".
    pub fn read_str<'a>(&self, ctx: &ExtractCtx<'a>) -> Option<Cow<'a, str>> {
        let raw = self.read_raw(ctx)?;
        let chain = self.sanitize();
        if chain.is_empty() {
            return Some(raw);
        }
        match eval_sanitize(chain, &raw)? {
            Value::String(s) => Some(Cow::Owned(s)),
            other => other.as_str().map(|s| Cow::Owned(s.to_owned())),
        }
    }

    /// The plain OSM key(s) this extract reads from — one for `Value`, the ordered candidate list
    /// for `Candidates`. Used by `categorize::linter`'s overlap grouping and
    /// `decision_tree`'s branch-key eligibility check (both only care about *which*
    /// tag(s) are read, not the `sanitize` chain).
    /// A virtual tag reports its `@` name (`parent_@id` when parent-scoped), so to these callers it
    /// is just one more opaque key.
    pub fn tag_names(&self) -> Vec<String> {
        match self {
            Extract::Value { key, .. } => vec![key.clone()],
            Extract::Candidates { keys, .. } => keys.clone(),
            Extract::Virtual { field, parent, .. } => {
                vec![if *parent { format!("parent_{}", field.name()) } else { field.name().to_owned() }]
            }
        }
    }

    /// This extract with every key prefixed (e.g. `"parent_"`), same `sanitize` chain — the
    /// `Filter::Parent` case: `lang::expr::prefix_expr_tags` models a parent-scoped
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
            Extract::Virtual { field, sanitize, .. } => {
                debug_assert_eq!(prefix, "parent_", "the parent scope is the only one a virtual tag has");
                Extract::Virtual { field: *field, parent: true, sanitize: sanitize.clone() }
            }
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
            Extract::Virtual { field, sanitize, .. } => {
                Extract::Virtual { field: *field, parent: false, sanitize: sanitize.clone() }
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

#[cfg(test)]
mod virtual_tag_tests {
    use serde_json::{json, Map, Value};

    use super::{Extract, VirtualKey};
    use crate::lang::filter::{eval, Filter};
    use crate::lang::producer::{Elem, ExtractCtx};
    use crate::lang::tags::{ElementKind, RawTags};

    fn extract(v: Value) -> Result<Extract, String> {
        serde_json::from_value(v).map_err(|e| e.to_string())
    }

    #[test]
    fn at_keys_resolve_at_load_time() {
        assert!(matches!(extract(json!({"tag": "@side"})), Ok(Extract::Virtual { field: VirtualKey::Side, parent: false, .. })));
        assert!(matches!(extract(json!({"key": "@id"})), Ok(Extract::Virtual { field: VirtualKey::Id, .. })));
        assert!(matches!(extract(json!({"tag": "highway"})), Ok(Extract::Value { .. })));
    }

    #[test]
    fn unknown_or_mixed_at_keys_are_load_errors() {
        let err = extract(json!({"tag": "@sied"})).unwrap_err();
        assert!(err.contains("unknown virtual tag \"@sied\""), "{err}");
        assert!(extract(json!({"keys": ["@id", "ref"]})).is_err());
    }

    /// Every virtual tag reads from the context, not the tags — and a tag literally named `@id`
    /// in `obj_tags` (impossible in practice) would not be what `@id` reads.
    #[test]
    fn values_come_from_the_context() {
        let tags: RawTags = [("@id".into(), "tag-value".into())].into_iter().collect();
        let mut ann = Map::new();
        ann.insert("_side".into(), json!("left"));
        ann.insert("_prefix".into(), json!("cycleway"));
        let ctx = ExtractCtx {
            obj_tags: &tags,
            parent_tags: Some(&tags),
            id: "way/5/cycleway/left",
            annotations: &ann,
            elem: Some(Elem { kind: ElementKind::Way, osm_id: 5 }),
            parent_elem: Some(Elem { kind: ElementKind::Relation, osm_id: 9 }),
        };
        let read = |k: &str| extract(json!({"tag": k})).unwrap().read_str(&ctx).map(|c| c.into_owned());
        assert_eq!(read("@id").as_deref(), Some("5"));
        assert_eq!(read("@type").as_deref(), Some("way"));
        assert_eq!(read("@side").as_deref(), Some("left"));
        assert_eq!(read("@prefix").as_deref(), Some("cycleway"));
        assert_eq!(read("@infix"), None);

        // Parent scope: the parent's identity, and no side annotations of the child.
        let parent = |f: Value| -> bool { eval(&serde_json::from_value::<Filter>(json!({"parent": f})).unwrap(), &ctx) };
        assert!(parent(json!({"tag": "@id", "eq": "9"})));
        assert!(parent(json!({"tag": "@type", "eq": "relation"})));
        assert!(parent(json!({"tag": "@side", "eq": "self"})));
        assert!(!parent(json!({"tag": "@prefix", "exists": true})));
    }

    /// The retired `{"annotation": "_side"}` spelling fails with a hint naming its replacement.
    #[test]
    fn annotation_spelling_is_a_load_error_with_a_hint() {
        let err = serde_json::from_value::<Filter>(json!({"annotation": "_side", "eq": "left"})).unwrap_err().to_string();
        assert!(err.contains(r#"{"tag": "@side", "eq": "left"}"#), "{err}");
    }

    /// An unsanitized `@side`/`@prefix`/`@infix` comparison compiles to the dedicated predicates
    /// the decision tree branches on — so trees are what the `annotation` spelling produced.
    #[test]
    fn side_comparisons_compile_to_the_side_predicates() {
        use crate::lang::expr::{filter_to_expr, Expr, Literal, Predicate};
        let expr = |v: Value| filter_to_expr(&serde_json::from_value::<Filter>(v).unwrap());
        assert!(matches!(expr(json!({"tag": "@side", "eq": "left"})), Expr::Lit(Literal::Pos(Predicate::Side(s))) if s == "left"));
        assert!(matches!(expr(json!({"tag": "@infix", "eq": ""})), Expr::Lit(Literal::Pos(Predicate::Infix(_)))));
        // In parent scope they are constants: a parent is never a side object.
        assert!(matches!(expr(json!({"parent": {"tag": "@side", "eq": "self"}})), Expr::True));
        assert!(matches!(expr(json!({"parent": {"tag": "@prefix", "eq": "cycleway"}})), Expr::False));
        // `@id` stays an ordinary comparison.
        assert!(matches!(expr(json!({"tag": "@id", "eq": "5"})), Expr::Lit(Literal::Pos(Predicate::Eq(..)))));
    }
}
