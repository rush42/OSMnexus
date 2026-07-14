//! The `Producer` engine — just `Match` and `Extract`, full stop — that evaluates one output's
//! value (the one mechanism behind every `outputs` entry, `TopicSpec::outputs`), its load-time
//! reference resolution (`resolve`), and the context (`ExtractCtx`/`TagSet`) and result
//! (`Produced`) types it evaluates over. `fallback` is JSON-only sugar, folded into an equivalent
//! `Match` by `tag_engine::parser`'s hand-written `Deserialize` impl — so it never exists as a
//! `Producer` value here, not pre-`resolve`, not transiently, not ever (see `parser`'s own doc for
//! why that folding lives in its own module rather than inline in this one). A named *shared*
//! classifier table (`{ "shared": "<name>" }`) isn't even sugar `Producer`/`parser` know about:
//! it's inlined as plain JSON — before anything here ever sees it — by `topic::load::
//! inline_shared_producers`, at topic-directory-read time, the same way shared macros/sanitizers
//! are merged in. `Producer` the Rust type really does only have two variants.

use std::collections::HashMap;

use serde::Deserialize;
use serde_json::{Map, Value};

use crate::tag_engine::extract::Extract;
use crate::tag_engine::filter::Filter;
use crate::tag_engine::keys;
use crate::tag_engine::sanitize::{resolve_sanitize, Sanitizer, SanitizeRef};
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

/// Which tagset `Producer::DirectedExtract` reads — the *only* producer left with a `from` field;
/// every other tagset-scoping need goes through `Producer::Parent`/`ParentOrObj` instead (see their
/// docs), since a directed read needs both `parent_tags` and the object's own `obj_tags`
/// simultaneously (to guard against overriding an already-set key) and so can't be expressed as a
/// plain "swap `obj_tags`, recurse" wrapper.
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

/// A value producer: `Match` (a rule table) or `Extract` (a leaf tag read) — see `tag_engine::
/// parser`'s hand-written `Deserialize` impl for the `fallback` JSON shape that folds into `Match`
/// at parse time and so never appears here. `Deserialize` isn't derived on this type itself —
/// deliberately, so a stray `#[derive(Deserialize)]` here can't reintroduce a shape `parser`
/// doesn't know about.
#[derive(Debug, Clone)]
pub enum Producer {
    /// A data-defined first-match-wins rule table (same engine as the `road` classifier). Each
    /// rule's value can be any JSON literal — a string (category/tag classification), a number
    /// (e.g. `minzoom`), a bool (e.g. a filter-driven flag) — or an arbitrary nested `Producer`
    /// (`ValueSpec::Producer`), which is what lets this one variant also subsume conditionals and
    /// ordered fallback chains: a rule matches when its `when` holds, and — if its value is itself
    /// a producer that produces nothing — matching doesn't stop the search, the next rule is
    /// tried (see `classifier::match_rules`). `consts` is the provenance a *literal*-valued rule
    /// contributes when it produces (a `Producer`-valued rule carries its own). With no `default`,
    /// returns `None` when no rule matches — letting a category const default or an enclosing
    /// fallback branch supply the value. Must be tried before `Extract` below, since `rules` is a
    /// required field and so unambiguously distinguishes it. Always reads `ctx.obj_tags` — wrap in
    /// `Parent`/`ParentOrObj` to read the parent way's tags instead.
    ///
    /// Rules see the same context a category condition does — tags, `side`/`prefix`/`infix`,
    /// parent, and macros (`as_category_context`) — so e.g. `{"prefix": "cycleway"}` or
    /// `{"macro": "..."}` work here exactly like in a category's own `condition`. The one
    /// remaining limitation: rules only see raw obj/parent tags, not fields derived earlier in the
    /// same pass.
    Match {
        rules: Vec<crate::tag_engine::classifier::Rule>,
        default: Option<Value>,
        consts: Map<String, Value>,
    },
    /// Plain tag read — always against `ctx.obj_tags` (wrap in `Parent`/`ParentOrObj` for the
    /// parent's tags).
    Extract {
        extract: Extract,
        /// Companion key/values this branch contributes when it produces the value; emitted as
        /// `<output>_<k>` (e.g. `{ "source": "tag", "confidence": "high" }`).
        consts: Map<String, Value>,
    },
    /// Direction-sensitive read of `key`: resolves its `:forward`/`:backward` variant from
    /// `ctx.split.obj_side` + the global left/right-hand-traffic setting
    /// (`traffic::is_left_hand_traffic`), producing nothing for a `self` object (no direction to
    /// resolve). Not expressible as a plain `Extract` wrapped in `Parent`: it needs both tagsets at
    /// once — the object's own (to guard against overriding an already-set key) and, when
    /// `from: Parent`, the parent's (tried bare-key-then-directed-key). Any other `from` tries only
    /// the directed variant on the object's own tags (e.g. a tag already unnested as
    /// `traffic_sign:forward`). Used for `split_sides`' `directed_keys`/`self_directed_keys` — the
    /// object-cardinality-changing split itself stays native, but this per-key projection is an
    /// ordinary sided tag read. Only ever constructed directly by `topic::runner`, never parsed
    /// from JSON.
    DirectedExtract {
        key: String,
        from: TagSet,
        sanitize: Option<SanitizeRef>,
        consts: Map<String, Value>,
    },
    /// Re-evaluate the inner producer against the parent way's tags instead of the object's own —
    /// `None` when there is no parent. The `Filter`-side sibling of this (`Filter::Parent`)
    /// documents the same shape in more detail.
    Parent(Box<Producer>),
    /// Like `Parent`, but falls back to the object's own tags when there is no parent (matches the
    /// old `TagSet::ParentOrObj`/yes_flag `source: parent`) — commits to the parent tagset whenever
    /// a parent exists, even if the inner producer then fails to find anything there; distinct from
    /// `{"fallback": [Parent(p), p]}`, which would also fall through to the object's tags when the
    /// parent merely lacks the key.
    ParentOrObj(Box<Producer>),
}

impl Producer {
    pub fn eval(&self, ctx: &ExtractCtx) -> Option<Produced> {
        match self {
            Producer::Match { rules, default, consts } => {
                crate::tag_engine::classifier::match_rules(rules, ctx, consts)
                    .or_else(|| default.clone().map(|value| Produced { value, consts: consts.clone() }))
            }

            Producer::Extract { extract, consts } => {
                let value = extract.read(ctx.obj_tags)?;
                Some(Produced { value, consts: consts.clone() })
            }

            Producer::DirectedExtract { key, from, sanitize, consts } => {
                if ctx.obj_tags.contains_key(key.as_str()) {
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
                        keys::first_present(tags, [key.as_str(), directed_key.as_str()])
                    }
                    _ => keys::first_present(ctx.obj_tags, [directed_key.as_str()]),
                }?;
                let value = resolve_sanitize(sanitize.as_ref(), raw)?;
                Some(Produced { value, consts: consts.clone() })
            }

            Producer::Parent(inner) => match ctx.parent_tags {
                None => None,
                Some(parent_tags) => inner.eval(&ExtractCtx { obj_tags: parent_tags, ..*ctx }),
            },

            Producer::ParentOrObj(inner) => {
                let tags = ctx.parent_tags.unwrap_or(ctx.obj_tags);
                inner.eval(&ExtractCtx { obj_tags: tags, ..*ctx })
            }
        }
    }
}

// ── Load-time resolution ──────────────────────────────────────────────────────

impl Producer {
    /// Resolve every named reference this producer (transitively) carries, once, at load time:
    /// macros embedded in a `Match`'s `rules[].when` (`Filter::expand`) and `Extract`'s
    /// `sanitize:` (`SanitizeRef::resolve`). After this, `eval` never does a registry lookup of
    /// any kind.
    pub fn resolve(
        &self,
        macros: &HashMap<String, Filter>,
        sanitizers: &HashMap<String, Sanitizer>,
    ) -> anyhow::Result<Producer> {
        Ok(match self {
            Producer::Match { rules, default, consts } => Producer::Match {
                rules: rules.iter()
                    .map(|r| Ok(crate::tag_engine::classifier::Rule {
                        when: r.when.expand(macros, sanitizers)?,
                        value: r.value.resolve(macros, sanitizers)?,
                    }))
                    .collect::<anyhow::Result<_>>()?,
                default: default.clone(),
                consts: consts.clone(),
            },
            Producer::Extract { extract, consts } => Producer::Extract {
                extract: extract.resolve(sanitizers)?,
                consts: consts.clone(),
            },
            Producer::DirectedExtract { key, from, sanitize, consts } => Producer::DirectedExtract {
                key: key.clone(),
                from: *from,
                sanitize: sanitize.as_ref().map(|r| r.resolve(sanitizers)).transpose()?,
                consts: consts.clone(),
            },
            Producer::Parent(inner) => Producer::Parent(Box::new(inner.resolve(macros, sanitizers)?)),
            Producer::ParentOrObj(inner) => Producer::ParentOrObj(Box::new(inner.resolve(macros, sanitizers)?)),
        })
    }
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
    /// `from` wraps it in `Parent`/`ParentOrObj` when not `TagSet::Obj`.
    fn bool_producer(filter: Filter, from: TagSet) -> Producer {
        let base = Producer::Match {
            rules: vec![Rule { when: filter, value: ValueSpec::Const(Value::Bool(true)) }],
            default: Some(Value::Bool(false)),
            consts: Map::new(),
        };
        match from {
            TagSet::Obj => base,
            TagSet::Parent => Producer::Parent(Box::new(base)),
            TagSet::ParentOrObj => Producer::ParentOrObj(Box::new(base)),
        }
    }

    #[test]
    fn matching_filter_produces_true() {
        let obj: RawTags = [("oneway".to_owned(), "yes".to_owned())].into_iter().collect();
        let producer = bool_producer(
            Filter::TagEq { extract: Extract { key: Some("oneway".to_owned()), keys: None, side: None, sanitize: None }, eq: "yes".to_owned() },
            TagSet::Obj,
        );
        let produced = producer.eval(&ctx(&obj, None)).unwrap();
        assert_eq!(produced.value, Value::Bool(true));
    }

    #[test]
    fn non_matching_filter_produces_false() {
        let obj: RawTags = [("oneway".to_owned(), "no".to_owned())].into_iter().collect();
        let producer = bool_producer(
            Filter::TagEq { extract: Extract { key: Some("oneway".to_owned()), keys: None, side: None, sanitize: None }, eq: "yes".to_owned() },
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
        Producer::DirectedExtract { key: key.to_owned(), from, sanitize: None, consts: Map::new() }
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
