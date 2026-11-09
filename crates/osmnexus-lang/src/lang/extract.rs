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

/// A key read plus its `sanitize` chain — a small recursive expression: three leaves (an OSM tag,
/// a fact, an annotation) and two combinators (first-present `Candidates`, `Parent` scope).
/// `Extract` is always `#[serde(flatten)]`ed into its embedding struct, so `sanitize` living here
/// rather than as a sibling field changes nothing about the on-disk JSON shape.
///
/// On disk: `{"key": k}` for one key, `{"keys": [..]}` for a first-present list whose entries are
/// either a key string or a full read (`{"key"|"keys": .., "sanitize"?}`), so candidate lists nest.
///
/// One prefix leaves the OSM tags, readable everywhere a tag is (filters, producers, `parent`,
/// `in`, candidate lists), and is resolved when the config loads so no element pays a string check
/// for it:
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
// An explicit one-byte tag instead of a niche in a `String`'s capacity: with several variants the
// niche-encoded discriminant takes ~8 instructions to decode on every read, which made a plain tag
// lookup measurably slower than when `Extract` had two variants. A byte compare doesn't.
#[repr(u8)]
pub enum Extract {
    /// First-present fallback: the first alternative that yields a value — after *its own*
    /// `sanitize`, so an alternative whose sanitizer drops the value counts as absent and the next
    /// one is tried — then run through this node's `sanitize`.
    Candidates { alts: Vec<Extract>, sanitize: Vec<Sanitizer> },
    /// A single OSM tag.
    Value { key: String, sanitize: Vec<Sanitizer> },
    /// A fact (`@id`, …).
    Virtual { field: VirtualKey, sanitize: Vec<Sanitizer> },
    /// An annotation, read from `ExtractCtx::annotations`. Never written directly:
    /// `{"annotations": ..}` turns every `Value` inside a read into this.
    Note { key: String, sanitize: Vec<Sanitizer> },
    /// The inner read, against the parent (`ExtractCtx::parent_scope`); absent without one. Never
    /// written directly either: a `Filter`/`Producer` `parent` wrapper switches scope itself, and
    /// `lang::expr::prefix_expr_tags` wraps each predicate's read in this when it flattens such a
    /// wrapper away for the decision tree.
    Parent(Box<Extract>),
}

/// The on-disk shapes, before `@` keys are resolved: `key` for one key, `keys` for a first-present
/// list. (The former aliases `tag`/`first_tag` are retired — see
/// `topic::sources::check_retired_syntax`.)
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum ExtractJson {
    Candidates {
        keys: Vec<Alt>,
        #[serde(default, deserialize_with = "crate::lang::json::deserialize_sanitize_chain")]
        sanitize: Vec<Sanitizer>,
    },
    Value {
        key: String,
        #[serde(default, deserialize_with = "crate::lang::json::deserialize_sanitize_chain")]
        sanitize: Vec<Sanitizer>,
    },
}

/// One entry of a `keys` list as written: a bare key (`"highway"`, `"@id"`) or a full read
/// (`{"key": "ref", "sanitize": ..}`, `{"keys": [..]}`). Kept unresolved until `TryFrom` (via
/// [`Extract::from_alts`]) so a nested error — an unknown fact — surfaces as itself rather than as
/// the enclosing untagged enum's "did not match any variant".
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum Alt {
    Key(String),
    Read(Box<ExtractJson>),
}

impl TryFrom<ExtractJson> for Extract {
    type Error = String;

    fn try_from(raw: ExtractJson) -> Result<Self, String> {
        match raw {
            ExtractJson::Value { key, sanitize } => Extract::key(key, sanitize),
            ExtractJson::Candidates { keys, sanitize } => Extract::from_alts(keys, sanitize),
        }
    }
}

impl Extract {
    /// A single-key read, resolving an `@` key to its fact. Every place that builds an `Extract`
    /// from a config-supplied key goes through this (directly or via [`Extract::keys`]), so `@`
    /// keys mean the same thing in every JSON shorthand.
    pub fn key(key: String, sanitize: Vec<Sanitizer>) -> Result<Extract, String> {
        if let Some(name) = key.strip_prefix('@') {
            return Ok(Extract::Virtual { field: VirtualKey::parse(name)?, sanitize });
        }
        Ok(Extract::Value { key, sanitize })
    }

    /// A first-present read over plain key strings — `Extract::key` for each, then
    /// [`Extract::candidates`]. Facts mix freely with tags: either may be absent.
    pub fn keys(keys: Vec<String>, sanitize: Vec<Sanitizer>) -> Result<Extract, String> {
        let alts = keys.into_iter().map(|k| Extract::key(k, Vec::new())).collect::<Result<_, _>>()?;
        Extract::candidates(alts, sanitize)
    }

    /// A first-present read over `keys` entries as written.
    pub fn from_alts(alts: Vec<Alt>, sanitize: Vec<Sanitizer>) -> Result<Extract, String> {
        let alts = alts
            .into_iter()
            .map(|a| match a {
                Alt::Key(k) => Extract::key(k, Vec::new()),
                Alt::Read(raw) => Extract::try_from(*raw),
            })
            .collect::<Result<_, _>>()?;
        Extract::candidates(alts, sanitize)
    }

    /// A first-present read over arbitrary alternatives.
    pub fn candidates(alts: Vec<Extract>, sanitize: Vec<Sanitizer>) -> Result<Extract, String> {
        if alts.is_empty() {
            return Err("`keys` needs at least one entry".to_owned());
        }
        Ok(Extract::Candidates { alts, sanitize })
    }

    /// This read, scoped to the annotations (`{"annotations": ..}`): every tag read inside it reads
    /// `ExtractCtx::annotations` instead. A fact stays a fact — it isn't a tag either.
    pub fn into_annotations(self) -> Extract {
        match self {
            Extract::Value { key, sanitize } => Extract::Note { key, sanitize },
            Extract::Candidates { alts, sanitize } => {
                Extract::Candidates { alts: alts.into_iter().map(Extract::into_annotations).collect(), sanitize }
            }
            Extract::Parent(inner) => Extract::Parent(Box::new(inner.into_annotations())),
            other @ (Extract::Virtual { .. } | Extract::Note { .. }) => other,
        }
    }

    /// This read, against the parent — what `lang::expr::prefix_expr_tags` makes of a predicate
    /// under a flattened `Filter::Parent`.
    pub fn in_parent(self) -> Extract {
        Extract::Parent(Box::new(self))
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

/// An annotation as text: a string as-is, anything else (a number or bool a transform recorded) in
/// its JSON spelling.
fn note_text<'a>(ctx: &ExtractCtx<'a>, key: &str) -> Option<Cow<'a, str>> {
    match ctx.annotations.get(key)? {
        Value::String(s) => Some(Cow::Borrowed(s.as_str())),
        Value::Null => None,
        other => Some(Cow::Owned(other.to_string())),
    }
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
    /// This node's own `sanitize` chain — empty if unset, and always empty for `Parent` (its inner
    /// read carries its own).
    pub fn sanitize(&self) -> &[Sanitizer] {
        match self {
            Extract::Value { sanitize, .. }
            | Extract::Candidates { sanitize, .. }
            | Extract::Virtual { sanitize, .. }
            | Extract::Note { sanitize, .. } => sanitize,
            Extract::Parent(_) => &[],
        }
    }

    /// Resolve the value *before this node's own* `sanitize`: a tag, a fact or an annotation as
    /// stored, the first alternative that yields one (each already through its own `sanitize`, as
    /// a string), or the inner read against the parent (sanitized — `Parent` has no chain of its
    /// own). Borrowed for every tag read; only `@id`/`@timestamp` and non-string annotations format
    /// a new string, and only when actually read.
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
            Extract::Candidates { alts, .. } => alts.iter().find_map(|a| a.read_text(ctx)),
            Extract::Virtual { field, .. } => field.read(ctx),
            Extract::Note { key, .. } => note_text(ctx, key),
            Extract::Parent(inner) => inner.read_str(&ctx.parent_scope()?),
        }
    }

    /// Read and run through `sanitize` (identity if unset) — what `Producer::Extract` produces.
    ///
    /// Hot path: a single plain tag, the overwhelmingly common case, is a hash lookup inlined into
    /// the caller. Every other shape — candidate lists, facts, annotations, parent reads — goes out
    /// of line, so supporting them costs a plain tag read nothing (they used to make this function
    /// too big for the lookup to be inlined into it, which measurably slowed every plain read).
    #[inline]
    pub fn read(&self, ctx: &ExtractCtx) -> Option<Value> {
        match self {
            Extract::Value { key, sanitize } => eval_sanitize(sanitize, ctx.obj_tags.get(key.as_str())?.as_ref()),
            _ => self.read_slow(ctx),
        }
    }

    #[inline(never)]
    fn read_slow(&self, ctx: &ExtractCtx) -> Option<Value> {
        match self {
            // The inner read's own value, not its string form: a sanitizer may produce a number.
            Extract::Parent(inner) => inner.read(&ctx.parent_scope()?),
            // Likewise the winning alternative's own value, when there's no chain of ours to run.
            Extract::Candidates { alts, sanitize } if sanitize.is_empty() => {
                alts.iter().find_map(|a| a.read(ctx).filter(|v| !v.is_null()))
            }
            _ => eval_sanitize(self.sanitize(), &self.read_raw_slow(ctx)?),
        }
    }

    /// What a `Candidates` alternative contributes: its fully sanitized value as text — a string
    /// as-is, any other value in its JSON spelling (as `note_text` does), so an alternative whose
    /// sanitizer yields a number still counts as present. Borrowed when there's nothing to run.
    fn read_text<'a>(&self, ctx: &ExtractCtx<'a>) -> Option<Cow<'a, str>> {
        match self {
            Extract::Parent(inner) => inner.read_text(&ctx.parent_scope()?),
            _ if self.sanitize().is_empty() => self.read_raw(ctx),
            _ => match self.read(ctx)? {
                Value::String(s) => Some(Cow::Owned(s)),
                Value::Null => None,
                other => Some(Cow::Owned(other.to_string())),
            },
        }
    }

    /// Like `read`, coerced to a string — what every `Filter` `Tag*` comparison reads (and what a
    /// `Candidates` alternative contributes). A dropped sanitize output (or one that isn't
    /// string-shaped) compares as absent, not equal to "".
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

    /// Whether this read looks at the parent — `decision_tree`'s check for whether a branch key or
    /// atom needs a parent at all. A variant compare, no allocation: it runs per predicate decision.
    pub fn is_parent_scoped(&self) -> bool {
        matches!(self, Extract::Parent(_))
    }

    /// A display name for one read: the key, `@id`, `annotations.<key>`, `parent_<name>` — and for
    /// a candidate list, its alternatives' names joined with `|`. Not `sanitize`-aware.
    pub fn label(&self) -> String {
        match self {
            Extract::Candidates { alts, .. } => alts.iter().map(Extract::label).collect::<Vec<_>>().join("|"),
            _ => self.tag_names().remove(0),
        }
    }

    /// The names this extract reads, every alternative of a candidate list in order (flattened
    /// through nested lists). Used by `categorize::linter`'s overlap grouping and the decision
    /// tree's tests (both only care about *which* keys are read, not the `sanitize` chains). A fact
    /// reports its `@` name, an annotation `annotations.<key>`, and a parent read prefixes
    /// `parent_`, so to these callers every read is just an opaque key.
    pub fn tag_names(&self) -> Vec<String> {
        match self {
            Extract::Value { key, .. } => vec![key.clone()],
            Extract::Candidates { alts, .. } => alts.iter().flat_map(Extract::tag_names).collect(),
            Extract::Virtual { field, .. } => vec![field.name().to_owned()],
            Extract::Note { key, .. } => vec![format!("annotations.{key}")],
            Extract::Parent(inner) => inner.tag_names().into_iter().map(|k| format!("parent_{k}")).collect(),
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
        assert!(matches!(extract(json!({"key": "@id"})), Ok(Extract::Virtual { field: VirtualKey::Id, .. })));
        assert!(matches!(extract(json!({"key": "@role"})), Ok(Extract::Virtual { field: VirtualKey::Role, .. })));
        // `_` means nothing: an ordinary tag read. Annotations are read via `{"annotations": ..}`.
        assert!(matches!(extract(json!({"key": "_side"})), Ok(Extract::Value { .. })));
        assert!(matches!(extract(json!({"key": "highway"})), Ok(Extract::Value { .. })));
    }

    #[test]
    fn unknown_facts_are_load_errors_even_nested() {
        let err = extract(json!({"key": "@side"})).unwrap_err();
        assert!(err.contains("unknown fact \"@side\""), "{err}");
        let err = extract(json!({"keys": ["ref", {"keys": ["name", "@side"]}]})).unwrap_err();
        assert!(err.contains("unknown fact \"@side\""), "{err}");
        assert!(extract(json!({"keys": []})).is_err());
        assert!(extract(json!({"keys": [3]})).is_err());
        assert!(extract(json!({"keys": [{"nope": 1}]})).is_err());
    }

    /// Candidate lists are lists of reads: facts mix with tags, entries nest and carry their own
    /// `sanitize`, and an entry whose sanitizer drops its value counts as absent.
    #[test]
    fn candidates_are_recursive() {
        let tags: RawTags = [("ref".into(), "B 96".into()), ("oneway".into(), "no".into()), ("lanes".into(), "two".into())]
            .into_iter()
            .collect();
        let ann = Map::new();
        let ctx = ExtractCtx {
            obj_tags: &tags,
            parent_tags: None,
            id: "way/5",
            annotations: &ann,
            elem: Some(Elem { kind: ElementKind::Way, osm_id: 5, timestamp: None }),
            parent_elem: None,
            role: None,
        };
        let read = |v: Value| extract(v).unwrap().read(&ctx);

        // A fact as a fallback, and as a first choice.
        assert_eq!(read(json!({"keys": ["name", "@id"]})), Some(json!("5")));
        assert_eq!(read(json!({"keys": ["@id", "ref"]})), Some(json!("5")));
        // A dropping sanitizer makes its entry absent: `oneway=no` is skipped, `ref` wins.
        let yes_only = json!({"mapping": {"yes": "oneway"}, "on_miss": "drop"});
        assert_eq!(read(json!({"keys": [{"key": "oneway", "sanitize": yes_only}, "ref"]})), Some(json!("B 96")));
        // Nested lists, and the outer chain runs on whichever entry won.
        let upper = json!({"replace": [{"from": "B", "to": "Bundesstraße"}]});
        assert_eq!(read(json!({"keys": [{"keys": ["name", "ref"]}, "@id"], "sanitize": upper})), Some(json!("Bundesstraße 96")));
        // An entry's non-string value survives: as itself without an outer chain, as text for one.
        let num = json!({"mapping": {"two": 2}, "on_miss": "drop"});
        assert_eq!(read(json!({"keys": [{"key": "lanes", "sanitize": num}]})), Some(json!(2.0)));
        let e = extract(json!({"keys": [{"key": "lanes", "sanitize": num}]})).unwrap();
        assert_eq!(e.read_str(&ctx).as_deref(), Some("2.0"));
        assert_eq!(super::super::filter::read_num(&e, &ctx), Some(2.0));
        // Nothing present: absent.
        assert_eq!(read(json!({"keys": ["name", "@role"]})), None);
        assert_eq!(e.label(), "lanes");
        assert_eq!(extract(json!({"keys": [{"keys": ["a", "@id"]}, "b"]})).unwrap().tag_names(), ["a", "@id", "b"]);
    }

    /// `Extract::Parent` — what a flattened `parent` wrapper becomes — reads the parent by itself.
    #[test]
    fn parent_reads_switch_scope_themselves() {
        let tags: RawTags = [("highway".into(), "cycleway".into())].into_iter().collect();
        let parent_tags: RawTags = [("highway".into(), "primary".into()), ("width".into(), "two".into())].into_iter().collect();
        let ann = Map::new();
        let with_parent = ExtractCtx {
            obj_tags: &tags,
            parent_tags: Some(&parent_tags),
            id: "way/5/way/9",
            annotations: &ann,
            elem: Some(Elem { kind: ElementKind::Way, osm_id: 5, timestamp: None }),
            parent_elem: Some(Elem { kind: ElementKind::Way, osm_id: 9, timestamp: None }),
            role: None,
        };
        let orphan = ExtractCtx { parent_tags: None, parent_elem: None, ..with_parent };
        let p = |v: Value| extract(v).unwrap().in_parent();
        assert_eq!(p(json!({"key": "highway"})).read_str(&with_parent).as_deref(), Some("primary"));
        assert_eq!(p(json!({"key": "@id"})).read_str(&with_parent).as_deref(), Some("9"));
        assert_eq!(p(json!({"key": "highway"})).read_str(&orphan), None);
        let num = json!({"mapping": {"two": 2}, "on_miss": "drop"});
        assert_eq!(p(json!({"key": "width", "sanitize": num})).read(&with_parent), Some(json!(2.0)));
        assert_eq!(super::super::filter::read_num(&p(json!({"key": "width", "sanitize": num})), &with_parent), Some(2.0));
        assert_eq!(p(json!({"keys": ["name", "@id"]})).tag_names(), ["parent_name", "parent_@id"]);
        assert!(p(json!({"key": "highway"})).is_parent_scoped());
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
