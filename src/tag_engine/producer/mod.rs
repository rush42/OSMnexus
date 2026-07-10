//! The runtime half of the tag engine: pure per-object evaluation over an already-resolved
//! `Producer`/`Filter`/etc — no disk I/O, no name lookups (every macro/sanitizer/shared-classifier
//! reference has already been substituted at load time, see `tag_engine::loader`).
//!
//! - `mod.rs` (this file): the `Producer` engine (`Extract`/`Fallback`/`Cond`/`Classify`/
//!   `SharedClassify`) that evaluates one field's value — shared by `osm_fields`, sanitizers, and
//!   derivers alike. Atomic `&str -> atomic` chain steps (`Step`, data-defined, plus the one
//!   built-in, `parse_length`) live here too, as the building blocks of a `sanitize:` chain.
//! - `filter`/`classifier`: the generic first-match-wins rule table and predicate AST underneath
//!   `Producer::Classify`, category matching, and `exclude_condition`.
//! - `categories`/`decision_tree`: the category data model and its priority-order pruning net.
//! - `keys`: generic tag-key selection helpers (`first_present`/`sided_keys`) shared by this
//!   module and `filter`.
//! - `topic_runner`: `PreCatStep`, the in-place tag-mutation step applied before categorization.
//! - `runner`: the per-element pipeline (`pre_cat_steps` → `exclude_condition` → `transform` →
//!   categorize → `producer` field evaluation).
//! - `transform`: object-cardinality-changing steps (center-line side-split) — the one thing that
//!   isn't a per-object field evaluation.

pub mod categories;
pub mod classifier;
pub mod decision_tree;
pub mod filter;
pub mod keys;
pub mod runner;
pub mod topic_runner;
pub mod transform;

use std::collections::HashMap;

use serde::Deserialize;
use serde_json::{Map, Value};

use crate::tag_engine::producer::filter::Filter;
use crate::osm::types::RawTags;

/// The identity atomic transform: the value a bare tag read produces when no `sanitize` is named.
/// Not a special case — every `Extract` terminates in exactly one atomic transform; absent
/// `sanitize` just means "the identity one," same as any named entry in `sanitizers.json`.
fn identity(raw: &str) -> Value {
    Value::String(raw.to_owned())
}

/// An `Extract`/`Filter` `sanitize` reference. `Name` is what raw JSON always deserializes into
/// (tried first, so a plain string never lands in `Inline`); `resolve` (called once at load time,
/// alongside `Filter::expand`/`Producer::expand_macros`) replaces it with the actual chain, so
/// `eval` never does a registry lookup — same "resolve names once at load" treatment macros get.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum SanitizeRef {
    Name(String),
    Inline(AtomicChain),
}

impl SanitizeRef {
    fn eval(&self, raw: &str) -> Option<Value> {
        match self {
            SanitizeRef::Inline(chain) => chain.eval(raw),
            SanitizeRef::Name(name) => {
                tracing::error!("unresolved sanitizer '{name}' reached eval — resolve should have run at load");
                None
            }
        }
    }
}

/// Evaluate a resolved `sanitize` reference against `raw`. `None` is the identity transform
/// (always succeeds) — see `SanitizeRef`.
pub fn resolve_sanitize(sanitize: Option<&SanitizeRef>, raw: &str) -> Option<Value> {
    match sanitize {
        None => Some(identity(raw)),
        Some(r) => r.eval(raw),
    }
}

/// A produced value plus optional provenance. The `consts` are arbitrary key/value pairs the
/// winning fallback branch (or a Rust deriver) contributes; each is emitted as `<field>_<k>`
/// (e.g. `source`/`confidence` → `<field>_source`/`<field>_confidence`).
#[derive(Debug, Clone)]
pub struct Produced {
    pub value: Value,
    pub consts: Map<String, Value>,
}

/// Per-object addressing: which tags, which side/prefix/infix, which category scope. `Copy` so a
/// producer can cheaply build a variant (e.g. swapping `obj_tags` to the parent) when re-running
/// itself against a different tagset.
#[derive(Clone, Copy)]
pub struct ExtractCtx<'a> {
    pub obj_tags: &'a RawTags,
    pub parent_tags: Option<&'a RawTags>,
    pub obj_side: &'a str,
    /// The prefix that produced this object (e.g. "cycleway"; `None` for the self object) and the
    /// infix that matched during side-splitting — a `Classify`/`SharedClassify`/`Cond` producer's
    /// rules (and `Filter::Prefix`/`Infix`) can condition on these exactly like a category
    /// condition can, since they're evaluated with this same `ExtractCtx`.
    pub prefix: Option<&'a str>,
    pub infix: Option<&'a str>,
}

#[derive(Debug, Deserialize, Clone, Copy, Default)]
#[serde(rename_all = "snake_case")]
pub enum TagSet {
    #[default]
    Obj,
    /// Strict parent way: nothing if the object has no parent (matches old osm `parent`).
    Parent,
    /// Parent way, falling back to the object's own tags when there is no parent
    /// (matches the old yes_flag `source: parent`). Commits to the parent tagset when a
    /// parent exists — distinct from a `fallback:[{parent},{obj}]`, which would also fall
    /// through when the parent merely lacks the key.
    ParentOrObj,
}

impl TagSet {
    /// Which tagset a producer reads. `Parent` is strict (None when the object has no parent);
    /// `ParentOrObj` falls back to the object's own tags.
    fn resolve<'a>(&self, ctx: &ExtractCtx<'a>) -> Option<&'a RawTags> {
        match self {
            TagSet::Obj => Some(ctx.obj_tags),
            TagSet::Parent => ctx.parent_tags,
            TagSet::ParentOrObj => Some(ctx.parent_tags.unwrap_or(ctx.obj_tags)),
        }
    }
}

/// A value producer. Untagged: object with `fallback` → Fallback; otherwise → Extract. (Order
/// matters — Extract's fields are all optional.)
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum Producer {
    Fallback { fallback: Vec<Producer> },
    /// A data-defined first-match-wins rule table (same engine as the `road` classifier). The
    /// value of any matching rule (or `default`, if given) can be any JSON literal — a string
    /// (category/tag classification), a number (e.g. `minzoom`), or a bool (e.g. a filter-driven
    /// flag) — so this one variant subsumes what used to be separate `FilterZoom`/`FilterMatch`
    /// producers. `from` picks the tagset the rules read (obj by default, or the parent), and
    /// `consts` is the provenance this branch contributes when it produces. With no `default`,
    /// returns `None` when no rule matches — letting a category const default or a later
    /// fallback branch supply the value; must be tried before `Extract` below, since `rules` is
    /// a required field and so unambiguously distinguishes it (`Extract`'s fields are all
    /// optional, so it would otherwise match — and silently produce nothing — first).
    ///
    /// Rules see the same context a category condition does — tags, `side`/`prefix`/`infix`,
    /// parent, and macros (`as_category_context`) — so e.g. `{"prefix": "cycleway"}` or
    /// `{"macro": "..."}` work here exactly like in a category's own `condition`. The one
    /// remaining limitation: rules only see raw obj/parent tags, not fields derived earlier in the
    /// same pass.
    Classify {
        rules: Vec<crate::tag_engine::producer::classifier::Rule>,
        #[serde(default)] default: Option<Value>,
        #[serde(default)] from: TagSet,
        #[serde(default)] consts: Map<String, Value>,
    },
    /// Like `Classify`, but the rule table is a named shared classifier loaded from
    /// `topics/_shared/classifiers/<shared>.json` — lets topics reuse one table (e.g. the `road`
    /// classification) without duplicating it in every topic's own JSON. `from`/`consts` behave
    /// as in `Classify`; the shared table's own `default` (if any) applies. Only exists
    /// pre-`resolve`: inlined into an equivalent `Classify` at load time (small tables — a
    /// handful of rules, referenced from a couple of topics — so cloning them per reference site
    /// is cheap, and it means nothing at eval time distinguishes a shared table from a topic-local
    /// one).
    SharedClassify {
        shared: String,
        #[serde(default)] from: TagSet,
        #[serde(default)] consts: Map<String, Value>,
    },
    /// Conditional producer selection: evaluate `cond` against this object's own `ExtractCtx` (same
    /// `Filter` machinery a category `condition` uses — tags, side, prefix, infix, macros), and
    /// produce from `then` if it holds, else from `r#else` (absent `r#else`
    /// means "produce nothing" when `cond` is false). Must come before `Extract` below, since
    /// `cond`/`then` are required fields and so unambiguously distinguish it (`Extract`'s fields
    /// are all optional, so it would otherwise match first).
    Cond {
        cond: Filter,
        then: Box<Producer>,
        #[serde(default)] r#else: Option<Box<Producer>>,
    },
    Extract {
        #[serde(default)] key: Option<String>,
        #[serde(default)] keys: Option<Vec<String>>,
        #[serde(default)] from: TagSet,
        #[serde(default)] side: Option<String>,
        #[serde(default)] sanitize: Option<SanitizeRef>,
        /// Companion key/values this branch contributes when it produces the value; emitted as
        /// `<output>_<k>` (e.g. `{ "source": "tag", "confidence": "high" }`).
        #[serde(default)] consts: Map<String, Value>,
        /// Direction-sensitive read (needs `key`, ignores `keys`/`side`): resolves `key`'s
        /// `:forward`/`:backward` variant from `ctx.obj_side` + the global left/right-hand-traffic
        /// setting (`traffic::is_left_hand_traffic`), producing nothing for a `self` object (no
        /// direction to resolve). `from: Parent` tries the bare key on the parent's tags, then its
        /// directed variant; any other `from` tries only the directed variant on the object's own
        /// tags (e.g. a tag already unnested as `traffic_sign:forward`). Used for `split_sides`'
        /// `directed_keys`/`self_directed_keys` — the object-cardinality-changing split itself
        /// stays native, but this per-key projection is an ordinary sided tag read.
        #[serde(default)] directed: bool,
    },
}

impl Producer {
    pub fn eval(&self, ctx: &ExtractCtx) -> Option<Produced> {
        match self {
            // First non-empty branch wins, carrying its own source/confidence.
            Producer::Fallback { fallback } => fallback.iter().find_map(|p| p.eval(ctx)),

            Producer::Classify { rules, default, from, consts } => {
                let tags = from.resolve(ctx)?;
                let mut rctx = *ctx;
                rctx.obj_tags = tags;
                crate::tag_engine::producer::classifier::classify_rules(rules, &rctx)
                    .or_else(|| default.clone())
                    .map(|value| Produced { value, consts: consts.clone() })
            }

            // Only reachable if `resolve` (which inlines this into `Classify`) was skipped —
            // kept working defensively rather than panicking.
            Producer::SharedClassify { shared, from, consts } => {
                let tags = from.resolve(ctx)?;
                let mut rctx = *ctx;
                rctx.obj_tags = tags;
                crate::tag_engine::loader::classifier::shared_classifier(shared)
                    .classify(&rctx)
                    .map(|value| Produced { value, consts: consts.clone() })
            }

            Producer::Cond { cond, then, r#else } => {
                if crate::tag_engine::producer::filter::eval(cond, ctx) {
                    then.eval(ctx)
                } else {
                    r#else.as_ref().and_then(|p| p.eval(ctx))
                }
            }

            Producer::Extract { key, keys: _, from, side: _, sanitize, consts, directed: true } => {
                let key = key.as_deref().expect("directed extract needs `key`");
                if ctx.obj_tags.contains_key(key) {
                    return None; // already set (e.g. by an earlier unnest) — don't override it
                }
                let suffix = match (ctx.obj_side, crate::traffic::is_left_hand_traffic()) {
                    ("left", false) | ("right", true) => ":backward",
                    ("right", false) | ("left", true) => ":forward",
                    _ => return None, // "self": no direction to resolve
                };
                let directed_key = format!("{key}{suffix}");
                let raw = match from {
                    TagSet::Parent => {
                        let tags = ctx.parent_tags?;
                        keys::first_present(tags, [key, directed_key.as_str()])
                    }
                    _ => keys::first_present(ctx.obj_tags, [directed_key.as_str()]),
                }?;
                let value = resolve_sanitize(sanitize.as_ref(), raw)?;
                Some(Produced { value, consts: consts.clone() })
            }

            Producer::Extract { key, keys, from, side, sanitize, consts, directed: false } => {
                let tags = from.resolve(ctx)?;
                let raw = read_raw(tags, key.as_deref(), keys.as_deref(), side.as_deref())?;
                let value = resolve_sanitize(sanitize.as_ref(), raw)?;
                Some(Produced { value, consts: consts.clone() })
            }
        }
    }
}

/// An atomic `&str -> atomic` chain: a single `Step`, or a `Vec<Step>` folded left (each step
/// consumes the previous string; the terminal step may yield any atomic `Value`). A bare string
/// step (`Step::Builtin`) is a chain-of-one alias to a built-in transform.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum AtomicChain {
    Chain(Vec<Step>),
    One(Step),
}

impl AtomicChain {
    fn eval(&self, raw: &str) -> Option<Value> {
        match self {
            AtomicChain::One(step) => step.apply(raw),
            AtomicChain::Chain(steps) => {
                let mut cur = Value::String(raw.to_owned());
                for s in steps {
                    cur = s.apply(cur.as_str()?)?;
                }
                Some(cur)
            }
        }
    }
}

// ── Chain steps: the atomic `&str -> atomic value` building blocks of an `AtomicChain` ──────

/// Accepts either `"foo"` or `["foo", "bar"]` in JSON.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum StrOrVec {
    One(String),
    Many(Vec<String>),
}

impl StrOrVec {
    pub(crate) fn into_vec(self) -> Vec<String> {
        match self {
            StrOrVec::One(s) => vec![s],
            StrOrVec::Many(v) => v,
        }
    }

    fn contains(&self, v: &str) -> bool {
        match self {
            StrOrVec::One(s) => s == v,
            StrOrVec::Many(vs) => vs.iter().any(|s| s == v),
        }
    }
}

/// One transform step: a lookup table or an allow-list.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum Step {
    /// Table lookup. Values may be any atomic JSON (string/bool/number) so a step can produce
    /// e.g. a boolean (`{ "yes": true }`). On a miss, `on_miss` decides: "keep" (passthrough),
    /// "drop"/absent (null), or any other string (a constant default).
    Mapping {
        mapping: HashMap<String, Value>,
        #[serde(default)]
        on_miss: Option<String>,
    },
    /// Inverted lookup shorthand: `{ "<output>": "<input>" | ["<input>", ...] }`. Collapses the
    /// common case of many inputs → one output.
    Cases {
        cases: HashMap<String, StrOrVec>,
        #[serde(default)]
        on_miss: Option<String>,
    },
    /// Keep the value iff it is in the set, else drop (sugar for an identity mapping + drop).
    Filter { filter: Vec<String> },
    /// Drop the value iff it is in the set, else keep — the reject-list counterpart to `filter`.
    /// Dropping short-circuits the chain (e.g. `{ "drop": [""] }` to discard empty input).
    Drop { drop: Vec<String> },
    /// Literal string rewrites, applied in order (each transforms the running value, then the
    /// next sees the result — sed-like). A general, country-agnostic alternative to a hardcoded
    /// normalizer (e.g. the former `traffic_sign` builtin). Never drops.
    Replace { replace: Vec<ReplaceRule> },
    /// A built-in Rust transform as a (terminal) chain step, e.g. `"parse_length"`. Lets a data
    /// chain end in an algorithmic, possibly non-string transform.
    Builtin(String),
}

/// One literal rewrite for a `replace` step.
#[derive(Debug, Clone, Deserialize)]
pub struct ReplaceRule {
    from: String,
    to: String,
    #[serde(default)]
    at: ReplaceAt,
}

/// Where a `ReplaceRule` matches: anywhere (replace every occurrence) or only as a prefix
/// (rewrite the leading `from`, keep the suffix; no-op when absent).
#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplaceAt {
    #[default]
    Anywhere,
    Prefix,
}

impl ReplaceRule {
    fn apply(&self, s: &str) -> String {
        match self.at {
            ReplaceAt::Anywhere => s.replace(&self.from, &self.to),
            ReplaceAt::Prefix => match s.strip_prefix(&self.from) {
                Some(rest) => format!("{}{rest}", self.to),
                None => s.to_owned(),
            },
        }
    }
}

impl Step {
    fn apply(&self, v: &str) -> Option<Value> {
        match self {
            Step::Mapping { mapping, on_miss } => match mapping.get(v) {
                Some(mapped) => Some(mapped.clone()),
                None => apply_on_miss(on_miss.as_deref(), v),
            },
            // Linear scan over the (typically short) case lists — no separate normalize-to-Mapping
            // pass needed; `cases` is authoring sugar, not a performance-sensitive hot path.
            Step::Cases { cases, on_miss } => {
                match cases.iter().find(|(_, inputs)| inputs.contains(v)) {
                    Some((output, _)) => Some(Value::String(output.clone())),
                    None => apply_on_miss(on_miss.as_deref(), v),
                }
            }
            Step::Filter { filter } => {
                filter.iter().any(|a| a == v).then(|| Value::String(v.to_owned()))
            }
            Step::Drop { drop } => {
                (!drop.iter().any(|a| a == v)).then(|| Value::String(v.to_owned()))
            }
            Step::Replace { replace } => {
                let out = replace.iter().fold(v.to_owned(), |s, r| r.apply(&s));
                Some(Value::String(out))
            }
            Step::Builtin(name) => apply_builtin(name, v),
        }
    }
}

/// Shared `on_miss` handling for `Mapping`/`Cases`: "keep" (passthrough), "drop"/absent (null),
/// or any other string (a constant default).
fn apply_on_miss(on_miss: Option<&str>, v: &str) -> Option<Value> {
    match on_miss {
        Some("keep") => Some(Value::String(v.to_owned())),
        Some("drop") | None => None,
        Some(constant) => Some(Value::String(constant.to_owned())),
    }
}

// ── Built-in registry ───────────────────────────────────────────────────

/// Apply a named built-in `&str -> atomic` transform. Returns None when the value is rejected
/// (not in an allowed set / unparseable).
pub fn apply_builtin(name: &str, raw: &str) -> Option<Value> {
    match name {
        "parse_length" => parse_length(raw).map(|v| Value::Number(float_to_json(v))),
        // `parse_length` is the lone built-in: universal unit arithmetic, not a finite table.
        // Everything else (incl. the former `traffic_sign` country normalizer) lives in
        // sanitizers.json — as mapping/cases/filter/replace chains.
        other => { tracing::warn!("unknown built-in atomic transform: {other}"); None }
    }
}

fn float_to_json(v: f32) -> serde_json::Number {
    serde_json::Number::from_f64(v as f64).unwrap_or_else(|| serde_json::Number::from(0))
}

// ── parse_length ──────────────────────────────────────────────────────────────

/// Converts OSM length strings to metres. Handles: "2.5", "2.5 m", "250 cm", "2500 mm", "8 ft",
/// "8'6\"", … — the general `parse_compound_unit` algorithm over the `"length"` unit table
/// (`_shared/units.json`); no unit-specific logic lives here.
pub fn parse_length(raw: &str) -> Option<f32> {
    crate::units::parse_compound_unit(raw, crate::units::unit_table("length"))
}

/// Resolve the raw string for an Extract — all three forms are a first-present fallback over a
/// candidate key list: a sided expansion (`key:{side}` → `:both` → bare-left), a single `key`,
/// or the explicit `keys` list.
fn read_raw<'a>(
    tags: &'a RawTags,
    key: Option<&str>,
    keys: Option<&[String]>,
    side: Option<&str>,
) -> Option<&'a str> {
    if let Some(side) = side {
        let candidates = keys::sided_keys(key.expect("sided extract needs `key`"), side, true);
        return keys::first_present(tags, candidates);
    }
    if let Some(key) = key {
        return keys::first_present(tags, std::iter::once(key));
    }
    if let Some(keys) = keys {
        return keys::first_present(tags, keys);
    }
    None
}

#[cfg(test)]
mod classify_bool_tests {
    use super::*;
    use crate::tag_engine::producer::classifier::{Rule, ValueSpec};
    use crate::tag_engine::producer::filter::Filter;

    fn ctx<'a>(obj: &'a RawTags, parent: Option<&'a RawTags>) -> ExtractCtx<'a> {
        ExtractCtx {
            obj_tags: obj,
            parent_tags: parent,
            obj_side: "self",
            prefix: None,
            infix: None,
        }
    }

    /// A `Classify` producer with one rule and a `default`, mirroring the old `FilterMatch` shape.
    fn bool_producer(filter: Filter, from: TagSet) -> Producer {
        Producer::Classify {
            rules: vec![Rule { when: filter, value: ValueSpec::Const(Value::Bool(true)) }],
            default: Some(Value::Bool(false)),
            from,
            consts: Map::new(),
        }
    }

    #[test]
    fn matching_filter_produces_true() {
        let obj: RawTags = [("oneway".to_owned(), "yes".to_owned())].into_iter().collect();
        let producer = bool_producer(
            Filter::TagEq { tag: "oneway".to_owned(), eq: "yes".to_owned(), sanitize: None },
            TagSet::Obj,
        );
        let produced = producer.eval(&ctx(&obj, None)).unwrap();
        assert_eq!(produced.value, Value::Bool(true));
    }

    #[test]
    fn non_matching_filter_produces_false() {
        let obj: RawTags = [("oneway".to_owned(), "no".to_owned())].into_iter().collect();
        let producer = bool_producer(
            Filter::TagEq { tag: "oneway".to_owned(), eq: "yes".to_owned(), sanitize: None },
            TagSet::Obj,
        );
        let produced = producer.eval(&ctx(&obj, None)).unwrap();
        assert_eq!(produced.value, Value::Bool(false));
    }

    #[test]
    fn missing_tagset_produces_none() {
        let obj = RawTags::default();
        let producer = bool_producer(Filter::Bool(true), TagSet::Parent);
        assert!(producer.eval(&ctx(&obj, None)).is_none());
    }
}

#[cfg(test)]
mod directed_extract_tests {
    use super::*;

    fn tags(pairs: &[(&str, &str)]) -> RawTags {
        pairs.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect()
    }

    fn ctx<'a>(obj: &'a RawTags, parent: Option<&'a RawTags>, obj_side: &'a str) -> ExtractCtx<'a> {
        ExtractCtx {
            obj_tags: obj, parent_tags: parent, obj_side,
            prefix: None, infix: None,
        }
    }

    fn directed(key: &str, from: TagSet) -> Producer {
        Producer::Extract {
            key: Some(key.to_owned()), keys: None, from, side: None, sanitize: None,
            consts: Map::new(), directed: true,
        }
    }

    #[test]
    fn parent_source_prefers_existing_obj_value() {
        let obj = tags(&[("cycleway:lanes", "existing")]);
        let parent = tags(&[("cycleway:lanes:forward", "lane")]);
        let producer = directed("cycleway:lanes", TagSet::Parent);
        assert!(producer.eval(&ctx(&obj, Some(&parent), "right")).is_none());
    }

    #[test]
    fn parent_source_falls_back_to_bare_then_directed_key() {
        let obj = RawTags::default();
        let parent = tags(&[("cycleway:lanes:forward", "lane")]);
        let producer = directed("cycleway:lanes", TagSet::Parent);
        let produced = producer.eval(&ctx(&obj, Some(&parent), "right")).unwrap();
        assert_eq!(produced.value, Value::String("lane".to_owned()));

        let obj = RawTags::default();
        let parent = RawTags::default();
        assert!(producer.eval(&ctx(&obj, Some(&parent), "right")).is_none());
    }

    #[test]
    fn self_source_reads_from_obj_own_directed_key() {
        let obj = tags(&[("traffic_sign:forward", "DE:1022-10")]);
        let producer = directed("traffic_sign", TagSet::Obj);
        let produced = producer.eval(&ctx(&obj, None, "right")).unwrap();
        assert_eq!(produced.value, Value::String("DE:1022-10".to_owned()));
    }

    #[test]
    fn noop_for_self_side() {
        let obj = RawTags::default();
        let parent = tags(&[("cycleway:lanes:forward", "lane")]);
        let producer = directed("cycleway:lanes", TagSet::Parent);
        assert!(producer.eval(&ctx(&obj, Some(&parent), "self")).is_none());
    }

    #[test]
    fn handedness_flips_suffix() {
        let obj = RawTags::default();
        let parent = tags(&[("cycleway:lanes:backward", "lane")]);
        let producer = directed("cycleway:lanes", TagSet::Parent);
        // Right-hand traffic (global default in tests): Side::Right reads `:forward`, not
        // `:backward` — so this should NOT match.
        assert!(producer.eval(&ctx(&obj, Some(&parent), "right")).is_none());
        let produced = producer.eval(&ctx(&obj, Some(&parent), "left")).unwrap();
        assert_eq!(produced.value, Value::String("lane".to_owned()));
    }
}
