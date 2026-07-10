//! The extraction layer: how a field's value is produced from a way's tags.
//!
//! A `Producer` is either an `Extract` (read a tag — single `key` or first-present `keys`,
//! from obj/parent/centerline, optional `side` expansion, optional `sanitize`), a `fallback`
//! over producers (first non-empty wins), or a `cond` (produce from one of two producers, gated
//! by a `Filter`). This one combinator subsumes the old obj-then-parent lookup, multi-key
//! lookup, the sided (`key:{side}`→`:both`→bare-left) lookup, and the
//! `surface:colour`/`surface:color` fallback.

use std::collections::HashMap;

use serde::Deserialize;
use serde_json::{Map, Value};

use crate::tag_engine::keys;
use crate::tag_engine::filter::{CategoryContext, Filter};
use crate::tag_engine::sanitize::{self, Step};
use crate::osm::types::RawTags;
use crate::output::types::Side;

/// A named atomic chain (a `sanitizers.json`-style entry, or any `Producer::Atomic`): resolves
/// `name` in `registry` and evaluates it against `raw`; an unknown name falls back to the
/// built-in registry (`sanitize::apply_builtin`) — same precedence `SanitizerRegistry` used to
/// implement, now just a lookup against a plain `Producer` map.
pub fn resolve_sanitize(registry: &HashMap<String, Producer>, name: &str, raw: &str) -> Option<Value> {
    match registry.get(name) {
        Some(p) => p.eval_atomic(raw),
        None => sanitize::apply_builtin(name, raw),
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
    /// The matched category's parking-inference scope (`both`/`directional`/none) and the
    /// transformed object's side — used by the `traffic_mode` derivers.
    pub parking_inference: Option<&'a str>,
    pub obj_side: &'a str,
    /// The prefix that produced this object (e.g. "cycleway"; `None` for the self object) and the
    /// infix that matched during side-splitting — same fields `CategoryContext` carries, so a
    /// `Classify`/`SharedClassify`/`Cond` producer's rules can condition on them exactly like a
    /// category condition can (`as_category_context` below).
    pub prefix: Option<&'a str>,
    pub infix: Option<&'a str>,
}

/// Topic-wide lookup tables, identical for every producer evaluated within one topic/pass — as
/// opposed to `ExtractCtx`, which describes the one object currently being evaluated. Kept as a
/// separate `Copy` param (rather than fields on `ExtractCtx`) so the two concerns — "where" vs.
/// "what tools are available" — don't get tangled.
#[derive(Clone, Copy)]
pub struct Env<'a> {
    /// Named atomic chains (`sanitizers.json`, data-defined) used to resolve `sanitize` names;
    /// an unknown name falls back to the built-in registry (`resolve_sanitize`). A separate map
    /// from a topic's `derivers.json` — despite sharing the `Producer` type, the two are different
    /// namespaces (e.g. bikelanes' `surface` names both an atomic sanitizer and a composite
    /// deriver — same name, different concepts, kept apart on purpose).
    pub sanitizers: &'a HashMap<String, Producer>,
    /// This kind's category macros (`categories/macros.json` + shared) — lets a `Classify`/`Cond`
    /// rule reference a `{"macro": "..."}` the same way a category condition can.
    pub macros: &'a HashMap<String, Filter>,
}

impl<'a> ExtractCtx<'a> {
    /// Build the richer `CategoryContext` a `Classify`/`SharedClassify`/`Cond` producer's rules
    /// need to see everything a category condition sees. `tags` points at whichever tagset `from`
    /// resolved (obj or parent) — but `side`/`prefix`/`infix` always describe *this* object, not
    /// the resolved tagset, and `parent_tags` is always the object's real parent (unaffected by
    /// which tagset the rules are reading).
    fn as_category_context(&self, tags: &'a RawTags, env: &Env<'a>) -> CategoryContext<'a> {
        CategoryContext {
            tags,
            side: match self.obj_side {
                "left" => Side::Left,
                "right" => Side::Right,
                _ => Side::Self_,
            },
            prefix: self.prefix,
            parent_highway: self.parent_tags.and_then(|t| t.get("highway")).map(String::as_str),
            parent_tags: self.parent_tags,
            infix: self.infix,
            sanitizers: env.sanitizers,
        }
    }
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
        rules: Vec<crate::tag_engine::classifier::Rule>,
        #[serde(default)] default: Option<Value>,
        #[serde(default)] from: TagSet,
        #[serde(default)] consts: Map<String, Value>,
    },
    /// Like `Classify`, but the rule table is a named shared classifier loaded from
    /// `topics/_shared/classifiers/<shared>.json` — lets topics reuse one table (e.g. the `road`
    /// classification) without duplicating it. `from`/`consts` behave as in `Classify`; the
    /// shared table's own `default` (if any) applies.
    SharedClassify {
        shared: String,
        #[serde(default)] from: TagSet,
        #[serde(default)] consts: Map<String, Value>,
    },
    /// Conditional producer selection: evaluate `cond` against this object's own tags (same
    /// `Filter`/`CategoryContext` machinery a category `condition` uses — tags, side, prefix,
    /// infix, macros), and produce from `then` if it holds, else from `r#else` (absent `r#else`
    /// means "produce nothing" when `cond` is false). Must come before `Extract` below, since
    /// `cond`/`then` are required fields and so unambiguously distinguish it (`Extract`'s fields
    /// are all optional, so it would otherwise match first).
    Cond {
        cond: Filter,
        then: Box<Producer>,
        #[serde(default)] r#else: Option<Box<Producer>>,
    },
    /// An atomic `&str -> atomic` transform chain — a `sanitizers.json`-style entry (an array of
    /// `Step`s, a single `Step` object, or a bare string alias to a built-in). Evaluated via
    /// `eval_atomic`, not `eval` — it has no tagset/side/prefix of its own; whatever calls it
    /// (`Extract`'s `sanitize` field, a `num`/`tag` `Filter` predicate's `sanitize`) supplies the
    /// one already-extracted value it runs on. Must come before `Extract` below for the same
    /// reason `Cond`/`Classify` do: a bare `{"mapping": ...}`-shaped step would otherwise silently
    /// match `Extract`'s all-optional fields first.
    Atomic(AtomicChain),
    Extract {
        #[serde(default)] key: Option<String>,
        #[serde(default)] keys: Option<Vec<String>>,
        #[serde(default)] from: TagSet,
        #[serde(default)] side: Option<String>,
        #[serde(default)] sanitize: Option<String>,
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
    pub fn eval(&self, ctx: &ExtractCtx, env: &Env) -> Option<Produced> {
        match self {
            // First non-empty branch wins, carrying its own source/confidence.
            Producer::Fallback { fallback } => fallback.iter().find_map(|p| p.eval(ctx, env)),

            Producer::Classify { rules, default, from, consts } => {
                let tags = from.resolve(ctx)?;
                let cctx = ctx.as_category_context(tags, env);
                crate::tag_engine::classifier::classify_rules(rules, &cctx, env.macros)
                    .or_else(|| default.clone())
                    .map(|value| Produced { value, consts: consts.clone() })
            }

            Producer::SharedClassify { shared, from, consts } => {
                let tags = from.resolve(ctx)?;
                let cctx = ctx.as_category_context(tags, env);
                crate::tag_engine::classifier::shared_classifier(shared)
                    .classify(&cctx, env.macros)
                    .map(|value| Produced { value, consts: consts.clone() })
            }

            Producer::Cond { cond, then, r#else } => {
                let cctx = ctx.as_category_context(ctx.obj_tags, env);
                if crate::tag_engine::filter::eval(cond, &cctx, env.macros) {
                    then.eval(ctx, env)
                } else {
                    r#else.as_ref().and_then(|p| p.eval(ctx, env))
                }
            }

            Producer::Atomic(_) => {
                tracing::warn!("Producer::Atomic has no tagset — call eval_atomic instead of eval");
                None
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
                let value = match sanitize {
                    Some(name) => resolve_sanitize(env.sanitizers, name, raw)?,
                    None => Value::String(raw.to_owned()),
                };
                Some(Produced { value, consts: consts.clone() })
            }

            Producer::Extract { key, keys, from, side, sanitize, consts, directed: false } => {
                let tags = from.resolve(ctx)?;
                let raw = read_raw(tags, key.as_deref(), keys.as_deref(), side.as_deref())?;
                let value = match sanitize {
                    Some(name) => resolve_sanitize(env.sanitizers, name, raw)?,
                    None => Value::String(raw.to_owned()),
                };
                Some(Produced { value, consts: consts.clone() })
            }
        }
    }

    /// Evaluate a `Producer::Atomic` chain against an already-extracted value. Only meaningful on
    /// `Atomic` — every other variant needs a tagset/context it doesn't have here (see `eval`).
    pub fn eval_atomic(&self, raw: &str) -> Option<Value> {
        match self {
            Producer::Atomic(chain) => chain.eval(raw),
            _ => {
                tracing::warn!("producer used as an atomic sanitizer isn't Producer::Atomic");
                None
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
    use crate::tag_engine::classifier::{Rule, ValueSpec};
    use crate::tag_engine::filter::Filter;

    fn ctx<'a>(obj: &'a RawTags, parent: Option<&'a RawTags>) -> ExtractCtx<'a> {
        ExtractCtx {
            obj_tags: obj,
            parent_tags: parent,
            parking_inference: None,
            obj_side: "self",
            prefix: None,
            infix: None,
        }
    }

    fn env<'a>(
        sanitizers: &'a HashMap<String, Producer>,
        macros: &'a HashMap<String, Filter>,
    ) -> Env<'a> {
        Env { sanitizers, macros }
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
        let (sanitizers, macros) = (HashMap::new(), HashMap::new());
        let producer = bool_producer(
            Filter::TagEq { tag: "oneway".to_owned(), eq: "yes".to_owned(), sanitize: None },
            TagSet::Obj,
        );
        let produced = producer.eval(&ctx(&obj, None), &env(&sanitizers, &macros)).unwrap();
        assert_eq!(produced.value, Value::Bool(true));
    }

    #[test]
    fn non_matching_filter_produces_false() {
        let obj: RawTags = [("oneway".to_owned(), "no".to_owned())].into_iter().collect();
        let (sanitizers, macros) = (HashMap::new(), HashMap::new());
        let producer = bool_producer(
            Filter::TagEq { tag: "oneway".to_owned(), eq: "yes".to_owned(), sanitize: None },
            TagSet::Obj,
        );
        let produced = producer.eval(&ctx(&obj, None), &env(&sanitizers, &macros)).unwrap();
        assert_eq!(produced.value, Value::Bool(false));
    }

    #[test]
    fn missing_tagset_produces_none() {
        let obj = RawTags::default();
        let (sanitizers, macros) = (HashMap::new(), HashMap::new());
        let producer = bool_producer(Filter::Bool(true), TagSet::Parent);
        assert!(producer.eval(&ctx(&obj, None), &env(&sanitizers, &macros)).is_none());
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
            obj_tags: obj, parent_tags: parent, parking_inference: None, obj_side,
            prefix: None, infix: None,
        }
    }

    fn env<'a>(
        sanitizers: &'a HashMap<String, Producer>,
        macros: &'a HashMap<String, Filter>,
    ) -> Env<'a> {
        Env { sanitizers, macros }
    }

    fn directed(key: &str, from: TagSet) -> Producer {
        Producer::Extract {
            key: Some(key.to_owned()), keys: None, from, side: None, sanitize: None,
            consts: Map::new(), directed: true,
        }
    }

    #[test]
    fn parent_source_prefers_existing_obj_value() {
        let (sanitizers, macros) = (HashMap::new(), HashMap::new());
        let obj = tags(&[("cycleway:lanes", "existing")]);
        let parent = tags(&[("cycleway:lanes:forward", "lane")]);
        let producer = directed("cycleway:lanes", TagSet::Parent);
        assert!(producer.eval(&ctx(&obj, Some(&parent), "right"), &env(&sanitizers, &macros)).is_none());
    }

    #[test]
    fn parent_source_falls_back_to_bare_then_directed_key() {
        let (sanitizers, macros) = (HashMap::new(), HashMap::new());
        let obj = RawTags::default();
        let parent = tags(&[("cycleway:lanes:forward", "lane")]);
        let producer = directed("cycleway:lanes", TagSet::Parent);
        let produced = producer.eval(&ctx(&obj, Some(&parent), "right"), &env(&sanitizers, &macros)).unwrap();
        assert_eq!(produced.value, Value::String("lane".to_owned()));

        let obj = RawTags::default();
        let parent = RawTags::default();
        assert!(producer.eval(&ctx(&obj, Some(&parent), "right"), &env(&sanitizers, &macros)).is_none());
    }

    #[test]
    fn self_source_reads_from_obj_own_directed_key() {
        let (sanitizers, macros) = (HashMap::new(), HashMap::new());
        let obj = tags(&[("traffic_sign:forward", "DE:1022-10")]);
        let producer = directed("traffic_sign", TagSet::Obj);
        let produced = producer.eval(&ctx(&obj, None, "right"), &env(&sanitizers, &macros)).unwrap();
        assert_eq!(produced.value, Value::String("DE:1022-10".to_owned()));
    }

    #[test]
    fn noop_for_self_side() {
        let (sanitizers, macros) = (HashMap::new(), HashMap::new());
        let obj = RawTags::default();
        let parent = tags(&[("cycleway:lanes:forward", "lane")]);
        let producer = directed("cycleway:lanes", TagSet::Parent);
        assert!(producer.eval(&ctx(&obj, Some(&parent), "self"), &env(&sanitizers, &macros)).is_none());
    }

    #[test]
    fn handedness_flips_suffix() {
        let (sanitizers, macros) = (HashMap::new(), HashMap::new());
        let obj = RawTags::default();
        let parent = tags(&[("cycleway:lanes:backward", "lane")]);
        let producer = directed("cycleway:lanes", TagSet::Parent);
        // Right-hand traffic (global default in tests): Side::Right reads `:forward`, not
        // `:backward` — so this should NOT match.
        assert!(producer.eval(&ctx(&obj, Some(&parent), "right"), &env(&sanitizers, &macros)).is_none());
        let produced = producer.eval(&ctx(&obj, Some(&parent), "left"), &env(&sanitizers, &macros)).unwrap();
        assert_eq!(produced.value, Value::String("lane".to_owned()));
    }
}

