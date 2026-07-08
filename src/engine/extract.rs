//! The extraction layer: how a field's value is produced from a way's tags.
//!
//! A `Producer` is either an `Extract` (read a tag — single `key` or first-present `keys`,
//! from obj/parent/centerline, optional `side` expansion, optional `sanitize`), a `fallback`
//! over producers (first non-empty wins), or a `derive` call. This one combinator subsumes
//! the old obj-then-parent lookup, multi-key lookup, the sided (`key:{side}`→`:both`→bare-left)
//! lookup, and the `surface:colour`/`surface:color` fallback.

use std::collections::HashMap;

use serde::Deserialize;
use serde_json::{Map, Value};

use crate::classify::filter::{eval_filter, MinzoomCase};
use crate::classify::sanitize::SanitizerRegistry;
use crate::classify::{derive, sanitize};
use crate::osm::types::RawTags;

/// A produced value plus optional provenance. The `consts` are arbitrary key/value pairs the
/// winning fallback branch (or a Rust deriver) contributes; each is emitted as `<field>_<k>`
/// (e.g. `source`/`confidence` → `<field>_source`/`<field>_confidence`).
#[derive(Debug, Clone)]
pub struct Produced {
    pub value: Value,
    pub consts: Map<String, Value>,
}

impl Produced {
    /// A bare value with no companion consts.
    fn bare(value: Value) -> Self {
        Produced { value, consts: Map::new() }
    }
}

/// Everything a producer might need to resolve a value. `Copy` so a deriver can cheaply build a
/// variant (e.g. swapping `obj_tags` to the parent) when re-evaluating a sibling producer.
#[derive(Clone, Copy)]
pub struct ExtractCtx<'a> {
    pub obj_tags: &'a RawTags,
    pub parent_tags: Option<&'a RawTags>,
    /// The matched category's parking-inference scope (`both`/`directional`/none) and the
    /// transformed object's side — used by the `traffic_mode` deriver.
    pub parking_inference: Option<&'a str>,
    pub obj_side: &'a str,
    /// Sanitizer registry (data-defined chains + built-ins) used to resolve `sanitize` names.
    pub sanitizers: &'a SanitizerRegistry,
    /// The topic's deriver library — lets a Rust deriver re-evaluate a sibling by name
    /// (e.g. `smoothness_parent` re-runs the base `smoothness` fallback against the parent).
    pub derivers: &'a HashMap<String, Producer>,
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

/// A value producer. Untagged: object with `fallback` → Fallback; with `derive` → Derive;
/// otherwise → Extract. (Order matters — Extract's fields are all optional.)
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum Producer {
    Fallback { fallback: Vec<Producer> },
    /// A Rust-backed deriver. `out_side` fixes the side for the per-side `traffic_mode` deriver.
    Derive {
        derive: String,
        #[serde(default)] out_side: Option<String>,
    },
    /// A data-defined first-match-wins rule table (same engine as the `road` classifier).
    /// `from` picks the tagset the rules read (obj by default, or the parent), and `consts` is the
    /// provenance this branch contributes when it produces. Returns `None` when no rule matches —
    /// letting a category const default or a later fallback branch supply the value.
    Classify {
        rules: Vec<crate::classify::classifier::Rule>,
        #[serde(default)] from: TagSet,
        #[serde(default)] consts: Map<String, Value>,
    },
    /// Like `Classify`, but the rule table is a named shared classifier loaded from
    /// `topics/_shared/classifiers/<shared>.json` — lets topics reuse one table (e.g. the `road`
    /// classification) without duplicating it. `from`/`consts` behave as in `Classify`.
    SharedClassify {
        shared: String,
        #[serde(default)] from: TagSet,
        #[serde(default)] consts: Map<String, Value>,
    },
    /// A constant zoom, or first-matching-`Filter`-case zoom with a default — the `minzoom`
    /// shape, reusing the same `Filter`/`eval_filter` engine as `Classify` (neutral, obj-only
    /// context; no macro support, matching `Classify`'s existing limitation). Must be tried
    /// before `Extract` below: `Extract`'s fields are all optional, so it would otherwise
    /// match (and silently produce nothing) before `default`/`rules` ever get a look-in.
    FilterZoom {
        default: i32,
        #[serde(default)] rules: Vec<MinzoomCase>,
        #[serde(default)] from: TagSet,
    },
    /// A boolean field driven by a Filter condition — reuses the same `Filter`/`eval_filter`
    /// engine as `exclude_condition`/category conditions/`FilterZoom`, but produces a plain
    /// bool value instead of gating a whole object. `from` picks the tagset (obj by default,
    /// or parent). Must be tried before `Extract` below, for the same reason as `FilterZoom`.
    ///
    /// Limitation: like `FilterZoom`/`Classify`, `eval_filter` only sees raw obj/parent tags,
    /// not fields derived earlier in the same pass.
    FilterMatch {
        filter: crate::classify::filter::Filter,
        #[serde(default)] from: TagSet,
    },
    Extract {
        #[serde(default)] key: Option<String>,
        #[serde(default)] keys: Option<Vec<String>>,
        #[serde(default)] from: TagSet,
        #[serde(default)] side: Option<String>,
        #[serde(default)] sanitize: Option<String>,
        /// Companion key/values this branch contributes when it produces the value; emitted as
        /// `<output>_<k>` (e.g. `{ "source": "tag", "confidence": "high" }`).
        #[serde(default)] consts: Map<String, Value>,
    },
}

impl Producer {
    pub fn eval(&self, ctx: &ExtractCtx) -> Option<Produced> {
        match self {
            // First non-empty branch wins, carrying its own source/confidence.
            Producer::Fallback { fallback } => fallback.iter().find_map(|p| p.eval(ctx)),

            Producer::Classify { rules, from, consts } => {
                let tags = from.resolve(ctx)?;
                crate::classify::classifier::classify_rules(
                    rules, tags, &HashMap::new(), ctx.sanitizers,
                ).map(|v| Produced { value: Value::String(v), consts: consts.clone() })
            }

            Producer::SharedClassify { shared, from, consts } => {
                let tags = from.resolve(ctx)?;
                crate::classify::classifier::shared_classifier(shared)
                    .classify(tags, &HashMap::new(), ctx.sanitizers)
                    .map(|v| Produced { value: Value::String(v), consts: consts.clone() })
            }

            Producer::Derive { derive, out_side } => match derive.as_str() {
                "traffic_mode" => {
                    let out_side = out_side.as_deref()
                        .expect("traffic_mode deriver needs `out_side`");
                    // Parking inference reads the underlying way's parking tags. For side
                    // objects that's the parent; for self objects the parent is absent but the
                    // object's own tags carry the (never-unnested) parking tags.
                    let parking_tags = ctx.parent_tags.unwrap_or(ctx.obj_tags);
                    derive::traffic_mode_side(
                        ctx.obj_tags, parking_tags, ctx.parking_inference, ctx.obj_side, out_side,
                        ctx.sanitizers,
                    ).map(|v| Produced::bare(Value::String(v)))
                }
                "smoothness_parent" => derive::smoothness_parent(ctx),
                other => { tracing::warn!("unknown deriver: {other}"); None }
            },

            Producer::Extract { key, keys, from, side, sanitize, consts } => {
                let tags = from.resolve(ctx)?;
                let raw = read_raw(tags, key.as_deref(), keys.as_deref(), side.as_deref())?;
                let value = match sanitize {
                    Some(name) => ctx.sanitizers.apply(name, raw)?,
                    None => Value::String(raw.to_owned()),
                };
                Some(Produced { value, consts: consts.clone() })
            }

            Producer::FilterZoom { default, rules, from } => {
                let tags = from.resolve(ctx)?;
                let zoom = rules
                    .iter()
                    .find(|case| eval_filter(&case.when, tags, &HashMap::new(), ctx.sanitizers))
                    .map(|case| case.zoom)
                    .unwrap_or(*default);
                Some(Produced::bare(Value::Number(zoom.into())))
            }

            Producer::FilterMatch { filter, from } => {
                let tags = from.resolve(ctx)?;
                Some(Produced::bare(Value::Bool(eval_filter(filter, tags, &HashMap::new(), ctx.sanitizers))))
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
        let candidates = sanitize::sided_keys(key.expect("sided extract needs `key`"), side, true);
        return sanitize::first_present(tags, candidates);
    }
    if let Some(key) = key {
        return sanitize::first_present(tags, std::iter::once(key));
    }
    if let Some(keys) = keys {
        return sanitize::first_present(tags, keys);
    }
    None
}

#[cfg(test)]
mod filter_match_tests {
    use super::*;
    use crate::classify::filter::Filter;

    fn ctx<'a>(obj: &'a RawTags, parent: Option<&'a RawTags>, sanitizers: &'a SanitizerRegistry, derivers: &'a HashMap<String, Producer>) -> ExtractCtx<'a> {
        ExtractCtx {
            obj_tags: obj,
            parent_tags: parent,
            parking_inference: None,
            obj_side: "self",
            sanitizers,
            derivers,
        }
    }

    #[test]
    fn matching_filter_produces_true() {
        let obj: RawTags = [("oneway".to_owned(), "yes".to_owned())].into_iter().collect();
        let sanitizers = SanitizerRegistry::new(HashMap::new());
        let derivers = HashMap::new();
        let producer = Producer::FilterMatch {
            filter: Filter::TagEq { tag: "oneway".to_owned(), eq: "yes".to_owned(), sanitize: None },
            from: TagSet::Obj,
        };
        let produced = producer.eval(&ctx(&obj, None, &sanitizers, &derivers)).unwrap();
        assert_eq!(produced.value, Value::Bool(true));
    }

    #[test]
    fn non_matching_filter_produces_false() {
        let obj: RawTags = [("oneway".to_owned(), "no".to_owned())].into_iter().collect();
        let sanitizers = SanitizerRegistry::new(HashMap::new());
        let derivers = HashMap::new();
        let producer = Producer::FilterMatch {
            filter: Filter::TagEq { tag: "oneway".to_owned(), eq: "yes".to_owned(), sanitize: None },
            from: TagSet::Obj,
        };
        let produced = producer.eval(&ctx(&obj, None, &sanitizers, &derivers)).unwrap();
        assert_eq!(produced.value, Value::Bool(false));
    }

    #[test]
    fn missing_tagset_produces_none() {
        let obj = RawTags::default();
        let sanitizers = SanitizerRegistry::new(HashMap::new());
        let derivers = HashMap::new();
        let producer = Producer::FilterMatch {
            filter: Filter::Bool(true),
            from: TagSet::Parent,
        };
        assert!(producer.eval(&ctx(&obj, None, &sanitizers, &derivers)).is_none());
    }
}
