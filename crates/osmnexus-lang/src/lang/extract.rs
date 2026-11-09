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
/// Two prefixes leave the OSM tags, readable everywhere a tag is (filters, producers, `parent`,
/// `in`), and are resolved when the config loads so no element pays a string check for them:
///
/// - `@` — a **fact** about the element ([`VirtualKey`]: `@id`, `@type`, `@role`, `@timestamp`),
///   from the data itself. An unknown `@` key is a load error.
///
/// Annotations — whatever the topic's own transforms wrote with `annotate`/`record_infix_as` — are
/// read by scoping an ordinary key read with `{"annotations": ..}` (like `parent`), which load-time
/// rewrites into [`Extract::Note`] (`Extract::into_annotations`). No key name is special: `_side`
/// is just the name tilda's config happens to write.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Deserialize)]
#[serde(try_from = "ExtractJson")]
// An explicit one-byte tag instead of a niche in a `String`'s capacity: with four variants the
// niche-encoded discriminant takes ~8 instructions to decode on every read, which made a plain tag
// lookup measurably slower than when `Extract` had two variants. A byte compare doesn't.
#[repr(u8)]
pub enum Extract {
    /// First-present fallback over an ordered candidate list.
    Candidates { keys: Vec<String>, sanitize: Vec<Sanitizer> },
    /// A single, specific key.
    Value { key: String, sanitize: Vec<Sanitizer> },
    /// A fact (`@id`, …). `parent` marks the parent-scoped form — the counterpart of a
    /// `parent_<key>` name (see `prefixed`).
    Virtual { field: VirtualKey, parent: bool, sanitize: Vec<Sanitizer> },
    /// An annotation read from `ExtractCtx::annotations` — first present of `keys`. Never written
    /// directly: `{"annotations": ..}` turns a `Value`/`Candidates` into this. `parent` as for
    /// `Virtual`.
    Note { keys: Vec<String>, parent: bool, sanitize: Vec<Sanitizer> },
}

/// The on-disk shapes, before `@`/`_` keys are resolved: `key` for one key, `keys` for a
/// first-present list. (The former aliases `tag`/`first_tag` are retired — see
/// `topic::sources::check_retired_syntax`.)
#[derive(Deserialize)]
#[serde(untagged)]
enum ExtractJson {
    Candidates {
        keys: Vec<String>,
        #[serde(default, deserialize_with = "crate::lang::json::deserialize_sanitize_chain")]
        sanitize: Vec<Sanitizer>,
    },
    Value {
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
        if let Some(name) = key.strip_prefix('@') {
            return Ok(Extract::Virtual { field: VirtualKey::parse(name)?, parent: false, sanitize });
        }
        Ok(Extract::Value { key, sanitize })
    }

    /// A first-present read over candidate keys. A fact can't be one of several candidates
    /// (candidates are keys the element might or might not carry), except as the only one.
    pub fn keys(mut keys: Vec<String>, sanitize: Vec<Sanitizer>) -> Result<Extract, String> {
        if keys.len() == 1 && keys[0].starts_with('@') {
            return Extract::key(keys.remove(0), sanitize);
        }
        if let Some(k) = keys.iter().find(|k| k.starts_with('@')) {
            return Err(format!("{k:?} is a fact, not a key the element may carry, so it can't be one of several candidate keys; read it on its own"));
        }
        Ok(Extract::Candidates { keys, sanitize })
    }

    /// This read, scoped to the annotations (`{"annotations": ..}`): the same key(s), read from
    /// `ExtractCtx::annotations` instead of the tags. A fact stays a fact — it isn't a tag either.
    pub fn into_annotations(self) -> Extract {
        match self {
            Extract::Value { key, sanitize } => Extract::Note { keys: vec![key], parent: false, sanitize },
            Extract::Candidates { keys, sanitize } => Extract::Note { keys, parent: false, sanitize },
            other @ (Extract::Virtual { .. } | Extract::Note { .. }) => other,
        }
    }
}

/// The facts: what the data says about the element being classified, spelled as `@`-prefixed keys
/// (the convention osmium uses for the same purpose) so they read with the ordinary tag syntax.
/// They are not tags: `has_key_prefix`, `tags_empty` and tag passthrough never see them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum VirtualKey {
    /// `@id` — the element's OSM id. Read through `parent`, the parent's id (a member way's
    /// relation, a side object's base way).
    Id,
    /// `@type` — `node` / `way` / `relation`.
    Type,
    /// `@role` — a member way's role in its parent relation (`forward`, `platform`, …; `""` for an
    /// empty role). Absent without a parent relation.
    Role,
    /// `@timestamp` — the element's last edit, `YYYY-MM-DDTHH:MM:SSZ`. Absent when the file
    /// carries no metadata.
    Timestamp,
}

impl VirtualKey {
    pub const ALL: [VirtualKey; 4] = [VirtualKey::Id, VirtualKey::Type, VirtualKey::Role, VirtualKey::Timestamp];

    pub fn name(self) -> &'static str {
        match self {
            VirtualKey::Id => "@id",
            VirtualKey::Type => "@type",
            VirtualKey::Role => "@role",
            VirtualKey::Timestamp => "@timestamp",
        }
    }

    fn parse(name: &str) -> Result<Self, String> {
        Self::ALL.into_iter().find(|v| &v.name()[1..] == name).ok_or_else(|| {
            let known: Vec<&str> = Self::ALL.iter().map(|v| v.name()).collect();
            format!("unknown fact \"@{name}\" (known: {})", known.join(", "))
        })
    }

    fn read<'a>(self, ctx: &ExtractCtx<'a>) -> Option<Cow<'a, str>> {
        match self {
            VirtualKey::Id => ctx.elem.map(|e| Cow::Owned(e.osm_id.to_string())),
            VirtualKey::Type => ctx.elem.map(|e| Cow::Borrowed(e.kind.id_prefix())),
            VirtualKey::Role => ctx.role.map(Cow::Borrowed),
            VirtualKey::Timestamp => ctx.elem.and_then(|e| e.timestamp).map(|t| Cow::Owned(iso8601_utc(t))),
        }
    }
}

/// The first present of `keys` among the annotations, as text: a string as-is, anything else (a
/// number or bool a transform recorded) in its JSON spelling.
fn note_text<'a>(ctx: &ExtractCtx<'a>, keys: &[String]) -> Option<Cow<'a, str>> {
    keys.iter().find_map(|key| match ctx.annotations.get(key)? {
        Value::String(s) => Some(Cow::Borrowed(s.as_str())),
        Value::Null => None,
        other => Some(Cow::Owned(other.to_string())),
    })
}

/// Unix seconds as `YYYY-MM-DDTHH:MM:SSZ` — the format the `meta` column's `updated_at` uses.
/// Howard Hinnant's `civil_from_days`, so the core needs no date library.
pub fn iso8601_utc(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let sod = secs.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", sod / 3600, sod % 3600 / 60, sod % 60)
}

impl Extract {
    /// This variant's own `sanitize` chain — empty if unset.
    pub fn sanitize(&self) -> &[Sanitizer] {
        match self {
            Extract::Value { sanitize, .. }
            | Extract::Candidates { sanitize, .. }
            | Extract::Virtual { sanitize, .. }
            | Extract::Note { sanitize, .. } => sanitize,
        }
    }

    /// Resolve the raw string — a first-present fallback over the candidate list, a single key, or
    /// a fact/annotation. Not yet run through `sanitize`. Borrowed for every tag read; only `@id`/
    /// `@timestamp` format a new string, and only when actually read.
    ///
    /// A parent-scoped `Virtual`/`Note` read here (not via `decision_tree`'s parent redirect) reads
    /// the parent: a `Filter`/`Producer` `Parent` wrapper has already swapped `ctx` to
    /// `ExtractCtx::parent_scope`, and the flag only exists for the decision tree's flattened form.
    #[inline]
    pub fn read_raw<'a>(&self, ctx: &ExtractCtx<'a>) -> Option<Cow<'a, str>> {
        match self {
            Extract::Value { key, .. } => ctx.obj_tags.get(key.as_str()).map(|v| Cow::Borrowed(v.as_ref())),
            _ => self.read_raw_slow(ctx),
        }
    }

    #[inline(never)]
    fn read_raw_slow<'a>(&self, ctx: &ExtractCtx<'a>) -> Option<Cow<'a, str>> {
        match self {
            Extract::Value { key, .. } => ctx.obj_tags.get(key.as_str()).map(|v| Cow::Borrowed(v.as_ref())),
            Extract::Candidates { keys, .. } => first_present(ctx.obj_tags, keys.iter().map(String::as_str)).map(Cow::Borrowed),
            Extract::Virtual { field, .. } => field.read(ctx),
            Extract::Note { keys, .. } => note_text(ctx, keys),
        }
    }

    /// Read and run through `sanitize` (identity if unset) — what `Producer::Extract` produces.
    ///
    /// Hot path: a single plain tag, the overwhelmingly common case, is a hash lookup inlined into
    /// the caller. Every other shape — candidate lists, facts, annotations — goes out of line, so
    /// supporting them costs a plain tag read nothing (they used to make this function too big for
    /// the lookup to be inlined into it, which measurably slowed every plain read).
    #[inline]
    pub fn read(&self, ctx: &ExtractCtx) -> Option<Value> {
        match self {
            Extract::Value { key, sanitize } => eval_sanitize(sanitize, ctx.obj_tags.get(key.as_str())?.as_ref()),
            _ => self.read_slow(ctx),
        }
    }

    #[inline(never)]
    fn read_slow(&self, ctx: &ExtractCtx) -> Option<Value> {
        eval_sanitize(self.sanitize(), &self.read_raw_slow(ctx)?)
    }

    /// Like `read`, coerced to a string — what every `Filter` `Tag*` comparison reads. A dropped
    /// sanitize output (or one that isn't string-shaped) compares as absent, not equal to "".
    /// Same hot path as `read`: an unsanitized single tag borrows straight out of `obj_tags`.
    #[inline]
    pub fn read_str<'a>(&self, ctx: &ExtractCtx<'a>) -> Option<Cow<'a, str>> {
        match self {
            Extract::Value { key, sanitize } if sanitize.is_empty() => {
                ctx.obj_tags.get(key.as_str()).map(|v| Cow::Borrowed(v.as_ref()))
            }
            _ => self.read_str_slow(ctx),
        }
    }

    #[inline(never)]
    fn read_str_slow<'a>(&self, ctx: &ExtractCtx<'a>) -> Option<Cow<'a, str>> {
        let raw = self.read_raw_slow(ctx)?;
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
            Extract::Note { keys, parent, .. } => {
                keys.iter().map(|k| if *parent { format!("parent_annotations.{k}") } else { format!("annotations.{k}") }).collect()
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
                debug_assert_eq!(prefix, "parent_", "the parent scope is the only one a fact has");
                Extract::Virtual { field: *field, parent: true, sanitize: sanitize.clone() }
            }
            Extract::Note { keys, sanitize, .. } => {
                debug_assert_eq!(prefix, "parent_", "the parent scope is the only one an annotation has");
                Extract::Note { keys: keys.clone(), parent: true, sanitize: sanitize.clone() }
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
            Extract::Note { keys, sanitize, .. } => Extract::Note { keys: keys.clone(), parent: false, sanitize: sanitize.clone() },
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

    use super::{iso8601_utc, Extract, VirtualKey};
    use crate::lang::filter::{eval, Filter};
    use crate::lang::producer::{Elem, ExtractCtx, Producer};
    use crate::lang::tags::{ElementKind, RawTags};

    fn extract(v: Value) -> Result<Extract, String> {
        serde_json::from_value(v).map_err(|e| e.to_string())
    }

    #[test]
    fn prefixed_keys_resolve_at_load_time() {
        assert!(matches!(extract(json!({"key": "@id"})), Ok(Extract::Virtual { field: VirtualKey::Id, parent: false, .. })));
        assert!(matches!(extract(json!({"key": "@role"})), Ok(Extract::Virtual { field: VirtualKey::Role, .. })));
        // `_` means nothing: an ordinary tag read. Annotations are read via `{"annotations": ..}`.
        assert!(matches!(extract(json!({"key": "_side"})), Ok(Extract::Value { .. })));
        assert!(matches!(extract(json!({"key": "highway"})), Ok(Extract::Value { .. })));
    }

    #[test]
    fn unknown_facts_and_mixed_candidates_are_load_errors() {
        let err = extract(json!({"key": "@side"})).unwrap_err();
        assert!(err.contains("unknown fact \"@side\""), "{err}");
        assert!(extract(json!({"keys": ["@id", "ref"]})).is_err());
        assert!(extract(json!({"keys": ["_side", "ref"]})).is_ok());
    }

    #[test]
    fn timestamps_format_like_the_meta_column() {
        assert_eq!(iso8601_utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(iso8601_utc(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(iso8601_utc(1_762_540_245), "2025-11-07T18:30:45Z");
        assert_eq!(iso8601_utc(-1), "1969-12-31T23:59:59Z");
    }

    /// Facts read from the element's identity, annotations from whatever the config wrote — and a
    /// tag literally named `@id` (impossible in practice) is not what `@id` reads.
    #[test]
    fn values_come_from_the_context() {
        let tags: RawTags =
            [("@id".into(), "tag-value".into()), ("_side".into(), "tag".into())].into_iter().collect();
        let mut ann = Map::new();
        ann.insert("_side".into(), json!("left"));
        ann.insert("_lanes".into(), json!(2));
        let ctx = ExtractCtx {
            obj_tags: &tags,
            parent_tags: Some(&tags),
            id: "way/5/relation/9",
            annotations: &ann,
            elem: Some(Elem { kind: ElementKind::Way, osm_id: 5, timestamp: Some(0) }),
            parent_elem: Some(Elem { kind: ElementKind::Relation, osm_id: 9, timestamp: None }),
            role: Some("forward"),
        };
        let read = |k: &str| extract(json!({"key": k})).unwrap().read_str(&ctx).map(|c| c.into_owned());
        assert_eq!(read("@id").as_deref(), Some("5"));
        assert_eq!(read("@type").as_deref(), Some("way"));
        assert_eq!(read("@role").as_deref(), Some("forward"));
        assert_eq!(read("@timestamp").as_deref(), Some("1970-01-01T00:00:00Z"));
        assert_eq!(read("_side").as_deref(), Some("tag"));

        let filter = |f: Value| -> bool { eval(&serde_json::from_value::<Filter>(f).unwrap(), &ctx) };
        let produce = |p: Value| serde_json::from_value::<Producer>(p).unwrap().eval(&ctx).map(|x| x.value);
        // Inside `annotations`, everything reads the annotations — keys, candidates, the context
        // predicates over them — and facts stay facts.
        assert!(filter(json!({"annotations": {"key": "_side", "eq": "left"}})));
        assert!(filter(json!({"annotations": {"key": "_lanes", "eq": "2"}})));
        assert!(filter(json!({"annotations": {"key": "_prefix", "exists": false}})));
        assert!(filter(json!({"annotations": {"keys": ["_prefix", "_side"], "in": ["left"]}})));
        assert!(filter(json!({"annotations": {"has_key_prefix": "_la"}})));
        assert!(!filter(json!({"annotations": {"has_key_prefix": "highway"}})));
        assert!(filter(json!({"annotations": {"tags_empty": false}})));
        assert!(filter(json!({"annotations": {"key": "@id", "eq": "5"}})));
        assert!(filter(json!({"annotations": {"not": {"or": [{"key": "_side", "eq": "right"}, {"key": "@id", "eq": "1"}]}}})));
        // ... and a nested `parent` looks at the parent's annotations: it has none.
        assert!(!filter(json!({"annotations": {"parent": {"key": "_side", "exists": true}}})));
        assert!(filter(json!({"annotations": {"parent": {"tags_empty": true}}})));
        assert!(!filter(json!({"annotations": {"parent": {"has_key_prefix": "_"}}})));
        assert!(filter(json!({"annotations": {"parent": {"key": "@id", "eq": "9"}}})));

        assert_eq!(produce(json!({"annotations": {"key": "_side"}})), Some(json!("left")));
        assert_eq!(produce(json!({"annotations": {"key": "_nope", "or": "fallback"}})), Some(json!("fallback")));
        assert_eq!(
            produce(json!({"annotations": {"match": [
                {"when": {"key": "_side", "eq": "right"}, "value": "r"},
                {"when": {"key": "_side", "eq": "left"}, "value": {"key": "_lanes"}}
            ]}})),
            Some(json!("2"))
        );

        // Parent scope: the parent's identity, none of the child's annotations, no role of its own.
        let parent = |f: Value| -> bool { eval(&serde_json::from_value::<Filter>(json!({"parent": f})).unwrap(), &ctx) };
        assert!(parent(json!({"key": "@id", "eq": "9"})));
        assert!(parent(json!({"key": "@type", "eq": "relation"})));
        assert!(!parent(json!({"key": "@timestamp", "exists": true})));
        assert!(parent(json!({"key": "_side", "eq": "tag"})));
        assert!(!parent(json!({"key": "@role", "exists": true})));
    }

    /// The retired `{"annotation": "_side"}` spelling fails with a hint naming its replacement.
    #[test]
    fn annotation_spelling_is_a_load_error_with_a_hint() {
        let err = serde_json::from_value::<Filter>(json!({"annotation": "_side", "eq": "left"})).unwrap_err().to_string();
        assert!(err.contains(r#"{"annotations": {"key": "_side", "eq": "left"}}"#), "{err}");
    }
}
