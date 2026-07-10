//! The `Producer` engine (`Extract`/`Fallback`/`Cond`/`Classify`/`SharedClassify`) that evaluates
//! one field's value — shared by `osm_fields`, sanitizers, and derivers alike — its load-time
//! reference resolution (`resolve`), and the context (`ExtractCtx`/`TagSet`) and result
//! (`Produced`) types it evaluates over.

use std::collections::HashMap;

use serde::Deserialize;
use serde_json::{Map, Value};

use crate::tag_engine::filter::Filter;
use crate::tag_engine::keys;
use crate::tag_engine::sanitize::{resolve_sanitize, AtomicChain, SanitizeRef};
use crate::tag_engine::transform::side_split::SplitContext;
use crate::osm::types::RawTags;

/// A produced value plus optional provenance. The `consts` are arbitrary key/value pairs the
/// winning fallback branch (or a Rust deriver) contributes; each is emitted as `<field>_<k>`
/// (e.g. `source`/`confidence` → `<field>_source`/`<field>_confidence`).
#[derive(Debug, Clone)]
pub struct Produced {
    pub value: Value,
    pub consts: Map<String, Value>,
}

/// Which tags (`obj_tags`, `parent_tags`), plus side-split addressing (`split` — see
/// `transform::side_split::SplitContext`, whose only non-trivial constructor is
/// `TransformedObject::extract_ctx`) and `id` — the row id for this object, defaulted to the
/// element's own id and overwritten by `get_transformed_objects` for a side object (e.g.
/// `"way/123/cycleway/left"`). `Copy` so a producer can cheaply build a variant (e.g. swapping
/// `obj_tags` to the parent) when re-running itself against a different tagset.
#[derive(Clone, Copy)]
pub struct ExtractCtx<'a> {
    pub obj_tags: &'a RawTags,
    pub parent_tags: Option<&'a RawTags>,
    pub split: SplitContext,
    pub id: &'a str,
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
                crate::tag_engine::classifier::classify_rules(rules, &rctx)
                    .or_else(|| default.clone())
                    .map(|value| Produced { value, consts: consts.clone() })
            }

            // Only reachable if `resolve` (which inlines this into `Classify`) was skipped —
            // kept working defensively rather than panicking.
            Producer::SharedClassify { shared, from, consts } => {
                let tags = from.resolve(ctx)?;
                let mut rctx = *ctx;
                rctx.obj_tags = tags;
                crate::tag_engine::classifier::shared_classifier(shared)
                    .classify(&rctx)
                    .map(|value| Produced { value, consts: consts.clone() })
            }

            Producer::Cond { cond, then, r#else } => {
                if crate::tag_engine::filter::eval(cond, ctx) {
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
                let suffix = match (ctx.split.obj_side, crate::traffic::is_left_hand_traffic()) {
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

// ── Load-time resolution ──────────────────────────────────────────────────────

impl Producer {
    /// Resolve every named reference this producer (transitively) carries, once, at load time:
    /// macros embedded in a `Classify`'s `rules[].when` or a `Cond`'s `cond` (`Filter::expand`),
    /// `Extract`'s `sanitize:` (`SanitizeRef::resolve`), and `SharedClassify` (inlined into an
    /// equivalent `Classify` — its rules go through the same macro/sanitize resolution as any
    /// other `Classify`'s). After this, `eval` never does a registry lookup of any kind.
    pub fn resolve(
        &self,
        macros: &HashMap<String, Filter>,
        sanitizers: &HashMap<String, AtomicChain>,
    ) -> anyhow::Result<Producer> {
        Ok(match self {
            Producer::Fallback { fallback } => Producer::Fallback {
                fallback: fallback.iter().map(|p| p.resolve(macros, sanitizers)).collect::<anyhow::Result<_>>()?,
            },
            Producer::Classify { rules, default, from, consts } => Producer::Classify {
                rules: rules.iter()
                    .map(|r| Ok(crate::tag_engine::classifier::Rule {
                        when: r.when.expand(macros, sanitizers)?,
                        value: r.value.clone(),
                    }))
                    .collect::<anyhow::Result<_>>()?,
                default: default.clone(),
                from: *from,
                consts: consts.clone(),
            },
            Producer::SharedClassify { shared, from, consts } => {
                let classifier = crate::tag_engine::classifier::shared_classifier(shared);
                let rules = classifier.rules.iter()
                    .map(|r| Ok(crate::tag_engine::classifier::Rule {
                        when: r.when.expand(macros, sanitizers)?,
                        value: r.value.clone(),
                    }))
                    .collect::<anyhow::Result<_>>()?;
                Producer::Classify {
                    rules,
                    default: classifier.default.clone(),
                    from: *from,
                    consts: consts.clone(),
                }
            }
            Producer::Cond { cond, then, r#else } => Producer::Cond {
                cond: cond.expand(macros, sanitizers)?,
                then: Box::new(then.resolve(macros, sanitizers)?),
                r#else: r#else.as_ref().map(|p| p.resolve(macros, sanitizers)).transpose()?.map(Box::new),
            },
            Producer::Extract { key, keys, from, side, sanitize, consts, directed } => Producer::Extract {
                key: key.clone(),
                keys: keys.clone(),
                from: *from,
                side: side.clone(),
                sanitize: sanitize.as_ref().map(|r| r.resolve(sanitizers)).transpose()?,
                consts: consts.clone(),
                directed: *directed,
            },
        })
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
        ExtractCtx { obj_tags: obj, parent_tags: parent, split: SplitContext::default(), id: "" }
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

    fn ctx<'a>(obj: &'a RawTags, parent: Option<&'a RawTags>, obj_side: &'static str) -> ExtractCtx<'a> {
        ExtractCtx {
            obj_tags: obj,
            parent_tags: parent,
            split: SplitContext { obj_side, prefix: None, infix: None },
            id: "",
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
