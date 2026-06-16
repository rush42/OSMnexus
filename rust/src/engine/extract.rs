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
                let tags = match from {
                    TagSet::Obj => Some(ctx.obj_tags),
                    TagSet::Parent => ctx.parent_tags, // strict: None when no parent
                    TagSet::ParentOrObj => Some(ctx.parent_tags.unwrap_or(ctx.obj_tags)),
                }?;
                crate::classify::classifier::classify_rules(
                    rules, tags, &HashMap::new(), ctx.sanitizers,
                ).map(|v| Produced { value: Value::String(v), consts: consts.clone() })
            }

            Producer::SharedClassify { shared, from, consts } => {
                let tags = match from {
                    TagSet::Obj => Some(ctx.obj_tags),
                    TagSet::Parent => ctx.parent_tags, // strict: None when no parent
                    TagSet::ParentOrObj => Some(ctx.parent_tags.unwrap_or(ctx.obj_tags)),
                }?;
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
                "smoothness_parent" => smoothness_parent(ctx),
                other => { tracing::warn!("unknown deriver: {other}"); None }
            },

            Producer::Extract { key, keys, from, side, sanitize, consts } => {
                let tags = match from {
                    TagSet::Obj => Some(ctx.obj_tags),
                    TagSet::Parent => ctx.parent_tags, // strict: None when no parent
                    TagSet::ParentOrObj => Some(ctx.parent_tags.unwrap_or(ctx.obj_tags)),
                }?;
                let raw = read_raw(tags, key.as_deref(), keys.as_deref(), side.as_deref())?;
                let value = match sanitize {
                    Some(name) => ctx.sanitizers.apply(name, raw)?,
                    None => Value::String(raw.to_owned()),
                };
                Some(Produced { value, consts: consts.clone() })
            }
        }
    }
}

/// `deriveBikelaneSmoothness`: re-evaluate the base `smoothness` fallback (the single source of
/// truth for the 4-source derivation + provenance) against own and parent tags, then copy the
/// parent's value under the Lua guards, prefixing its source with `parent_highway_`.
fn smoothness_parent(ctx: &ExtractCtx) -> Option<Produced> {
    let base = ctx.derivers.get("smoothness")?;
    let own = base.eval(ctx);

    let Some(parent) = ctx.parent_tags else { return own };
    let mut pctx = *ctx;
    pctx.obj_tags = parent;
    let par = base.eval(&pctx);
    if par.is_none() {
        return own;
    }

    let own_surface = ctx.obj_tags.get("surface");
    let surfaces_match = own_surface == parent.get("surface");
    let own_source = own.as_ref().and_then(|p| p.consts.get("source")).and_then(Value::as_str);
    let own_from_tag = matches!(own_source, Some("tag") | Some("tag_normalized"));

    // A: own absent, and own surface absent or equal to the parent's.
    let cond_a = own.is_none() && (own_surface.is_none() || surfaces_match);
    // B: own not tag-sourced (derived or absent), own surface present and equal.
    let cond_b = !own_from_tag && own_surface.is_some() && surfaces_match;

    if cond_a || cond_b {
        par.map(|mut p| {
            // Prefix the copied source with `parent_highway_`.
            if let Some(s) = p.consts.get("source").and_then(Value::as_str) {
                let prefixed = Value::String(format!("parent_highway_{s}"));
                p.consts.insert("source".into(), prefixed);
            }
            p
        })
    } else {
        own
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
