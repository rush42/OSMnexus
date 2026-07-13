//! The `Producer` engine — just `Match` and `Extract` at eval time — that evaluates one output's
//! value (the one mechanism behind every `outputs` entry, `TopicSpec::outputs`), its load-time
//! reference resolution (`resolve`), and the context (`ExtractCtx`/`TagSet`) and result
//! (`Produced`) types it evaluates over. `Fallback` and `SharedClassify` are parse-time sugar
//! only: `resolve` always runs before `eval` (see `topic::runner`), and it rewrites both into an
//! equivalent `Match` — so `eval` never has to know they exist.

use std::collections::HashMap;

use serde::Deserialize;
use serde_json::{Map, Value};

use crate::tag_engine::filter::Filter;
use crate::tag_engine::keys;
use crate::tag_engine::sanitize::{resolve_sanitize, AtomicChain, SanitizeRef};
use crate::tag_engine::transform::side_split::SplitContext;
use crate::osm::types::RawTags;

/// A produced value plus optional provenance. The `consts` are arbitrary key/value pairs the
/// winning fallback branch contributes; each is emitted as `<field>_<k>` (e.g.
/// `source`/`confidence` → `<field>_source`/`<field>_confidence`).
#[derive(Debug, Clone)]
pub struct Produced {
    pub value: Value,
    pub consts: Map<String, Value>,
}

/// Which tags (`obj_tags`, `parent_tags`), plus side-split addressing (`split` — see
/// `transform::side_split::SplitContext`, whose only non-trivial constructor is
/// `side_split::generate_sides`) and `id` — the row id for this object, defaulted to the
/// element's own id and overwritten by `generate_sides` for a side object (e.g.
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

/// A value producer. Untagged, tried in this order (more-specific/required-field shapes before
/// `Extract`, whose fields are all optional and so would otherwise match everything first):
/// `Fallback` (`fallback` key) and `SharedClassify` (`shared` key) are pure JSON sugar, rewritten
/// by `resolve` into an equivalent `Match`; `Match` (`rules` key) and `Extract` are the two shapes
/// `eval` actually implements.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum Producer {
    /// Sugar for an all-`when:true` `Match`: try each branch in order, first one that produces
    /// anything wins, carrying its own branch-level `consts`. Only exists pre-`resolve`.
    Fallback { fallback: Vec<Producer> },
    /// Sugar for a `Match` whose rule table is a named shared classifier loaded from
    /// `<config_root>/producers.json` — lets topics reuse one table (e.g. the `road`
    /// classification) without duplicating it in every topic's own JSON. `from`/`consts` behave
    /// as in `Match`; the shared table's own `default` (if any) applies. Only exists
    /// pre-`resolve`: inlined into an equivalent `Match` at load time (small tables — a
    /// handful of rules, referenced from a couple of topics — so cloning them per reference site
    /// is cheap).
    SharedClassify {
        shared: String,
        #[serde(default)] from: TagSet,
        #[serde(default)] consts: Map<String, Value>,
    },
    /// A data-defined first-match-wins rule table (same engine as the `road` classifier). Each
    /// rule's value can be any JSON literal — a string (category/tag classification), a number
    /// (e.g. `minzoom`), a bool (e.g. a filter-driven flag) — or an arbitrary nested `Producer`
    /// (`ValueSpec::Producer`), which is what lets this one variant also subsume conditionals and
    /// ordered fallback chains: a rule matches when its `when` holds, and — if its value is itself
    /// a producer that produces nothing — matching doesn't stop the search, the next rule is
    /// tried (see `classifier::match_rules`). `from` picks the tagset rules read (obj by default,
    /// or the parent); `consts` is the provenance a *literal*-valued rule contributes when it
    /// produces (a `Producer`-valued rule carries its own). With no `default`, returns `None` when
    /// no rule matches — letting a category const default or an enclosing fallback branch supply
    /// the value. Must be tried before `Extract` below, since `rules` is a required field and so
    /// unambiguously distinguishes it.
    ///
    /// Rules see the same context a category condition does — tags, `side`/`prefix`/`infix`,
    /// parent, and macros (`as_category_context`) — so e.g. `{"prefix": "cycleway"}` or
    /// `{"macro": "..."}` work here exactly like in a category's own `condition`. The one
    /// remaining limitation: rules only see raw obj/parent tags, not fields derived earlier in the
    /// same pass.
    Match {
        rules: Vec<crate::tag_engine::classifier::Rule>,
        #[serde(default)] default: Option<Value>,
        #[serde(default)] from: TagSet,
        #[serde(default)] consts: Map<String, Value>,
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
            // Only reachable if `resolve` (which rewrites both into an equivalent `Match`) was
            // skipped — kept working defensively rather than panicking.
            Producer::Fallback { fallback } => fallback.iter().find_map(|p| p.eval(ctx)),
            Producer::SharedClassify { shared, from, consts } => {
                let classifier = crate::tag_engine::classifier::shared_classifier(shared);
                let tags = from.resolve(ctx)?;
                let mut rctx = *ctx;
                rctx.obj_tags = tags;
                crate::tag_engine::classifier::match_rules(&classifier.rules, &rctx, consts)
                    .or_else(|| classifier.default.clone().map(|value| Produced { value, consts: consts.clone() }))
            }

            Producer::Match { rules, default, from, consts } => {
                let tags = from.resolve(ctx)?;
                let mut rctx = *ctx;
                rctx.obj_tags = tags;
                crate::tag_engine::classifier::match_rules(rules, &rctx, consts)
                    .or_else(|| default.clone().map(|value| Produced { value, consts: consts.clone() }))
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
    /// Resolve every named reference this producer (transitively) carries, once, at load time,
    /// and collapse the JSON sugar (`Fallback`, `SharedClassify`) down to a plain `Match` — so
    /// `eval` only ever sees `Match`/`Extract`: macros embedded in a `Match`'s `rules[].when`
    /// (`Filter::expand`), `Extract`'s `sanitize:` (`SanitizeRef::resolve`), `SharedClassify`
    /// (its rules go through the same macro/sanitize resolution as a topic-local `Match`'s), and
    /// `Fallback` (each branch becomes an unconditional — `when: true` — rule wrapping that
    /// branch's own resolved producer as its value, so it keeps contributing its own `consts`;
    /// see `classifier::match_rules`). After this, `eval` never does a registry lookup of any
    /// kind.
    pub fn resolve(
        &self,
        macros: &HashMap<String, Filter>,
        sanitizers: &HashMap<String, AtomicChain>,
    ) -> anyhow::Result<Producer> {
        Ok(match self {
            Producer::Fallback { fallback } => Producer::Match {
                rules: fallback.iter()
                    .map(|p| Ok(crate::tag_engine::classifier::Rule {
                        when: Filter::Bool(true),
                        value: crate::tag_engine::classifier::ValueSpec::Producer(
                            Box::new(p.resolve(macros, sanitizers)?)
                        ),
                    }))
                    .collect::<anyhow::Result<_>>()?,
                default: None,
                from: TagSet::Obj,
                consts: Map::new(),
            },
            Producer::Match { rules, default, from, consts } => Producer::Match {
                rules: rules.iter()
                    .map(|r| Ok(crate::tag_engine::classifier::Rule {
                        when: r.when.expand(macros, sanitizers)?,
                        value: r.value.resolve(macros, sanitizers)?,
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
                        value: r.value.resolve(macros, sanitizers)?,
                    }))
                    .collect::<anyhow::Result<_>>()?;
                Producer::Match {
                    rules,
                    default: classifier.default.clone(),
                    from: *from,
                    consts: consts.clone(),
                }
            }
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

    /// A `Match` producer with one rule and a `default`, mirroring the old `FilterMatch` shape.
    fn bool_producer(filter: Filter, from: TagSet) -> Producer {
        Producer::Match {
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
