//! The extraction layer: how a field's value is produced from a way's tags.
//!
//! A `Producer` is either an `Extract` (read a tag — single `key` or first-present `keys`,
//! from obj/parent/centerline, optional `side` expansion, optional `sanitize`), a `fallback`
//! over producers (first non-empty wins), or a `derive` call. This one combinator subsumes
//! the old obj-then-parent lookup, multi-key lookup, `get_sided_with_bare_left`, and the
//! `surface:colour`/`surface:color` fallback.

use std::collections::HashMap;

use serde::Deserialize;
use serde_json::Value;

use crate::classify::sanitize::SanitizerRegistry;
use crate::classify::{derive, sanitize};
use crate::osm::types::RawTags;

/// A produced value plus optional provenance. `source`/`confidence` flow from the winning
/// fallback branch (or a Rust deriver) and are emitted as `<field>_source`/`<field>_confidence`.
#[derive(Debug, Clone)]
pub struct Produced {
    pub value: Value,
    pub source: Option<String>,
    pub confidence: Option<String>,
}

impl Produced {
    /// A bare value with no provenance.
    fn bare(value: Value) -> Self {
        Produced { value, source: None, confidence: None }
    }
}

/// Everything a producer might need to resolve a value. `Copy` so a deriver can cheaply build a
/// variant (e.g. swapping `obj_tags` to the parent) when re-evaluating a sibling producer.
#[derive(Clone, Copy)]
pub struct ExtractCtx<'a> {
    pub obj_tags: &'a RawTags,
    pub parent_tags: Option<&'a RawTags>,
    pub centerline_tags: &'a RawTags,
    /// Matched category id and the transformed object's side — used by the `traffic_mode` deriver.
    pub category_id: &'a str,
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
    /// (matches the old yes_flag `source: parent`).
    ParentOrObj,
    /// The center-line / parent way tags (always present).
    Centerline,
}

/// A value producer. Untagged: object with `fallback` → Fallback; with `derive` → Derive;
/// otherwise → Extract. (Order matters — Extract's fields are all optional.)
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum Producer {
    Fallback { fallback: Vec<Producer> },
    /// A Rust-backed deriver. `out_side` fixes the side for the per-side `traffic_mode` deriver;
    /// `implicit` selects implicit-one-way assumption for the `oneway` deriver.
    Derive {
        derive: String,
        #[serde(default)] out_side: Option<String>,
        #[serde(default)] implicit: bool,
    },
    Extract {
        #[serde(default)] key: Option<String>,
        #[serde(default)] keys: Option<Vec<String>>,
        #[serde(default)] from: TagSet,
        #[serde(default)] side: Option<String>,
        #[serde(default)] sanitize: Option<String>,
        /// Provenance this branch implies when it produces the value (Lua's *_source/_confidence).
        #[serde(default)] source: Option<String>,
        #[serde(default)] confidence: Option<String>,
    },
}

impl Producer {
    pub fn eval(&self, ctx: &ExtractCtx) -> Option<Produced> {
        match self {
            // First non-empty branch wins, carrying its own source/confidence.
            Producer::Fallback { fallback } => fallback.iter().find_map(|p| p.eval(ctx)),

            Producer::Derive { derive, out_side, implicit } => match derive.as_str() {
                "oneway" => Some(Produced::bare(Value::String(
                    derive::derive_oneway(ctx.obj_tags, *implicit),
                ))),
                "traffic_mode" => {
                    let out_side = out_side.as_deref()
                        .expect("traffic_mode deriver needs `out_side`");
                    derive::traffic_mode_side(
                        ctx.obj_tags, ctx.centerline_tags, ctx.category_id, ctx.obj_side, out_side,
                        ctx.sanitizers,
                    ).map(|v| Produced::bare(Value::String(v)))
                }
                "surface" => surface_produced(derive::surface(ctx.obj_tags, ctx.sanitizers), false),
                "surface_parent" => surface_parent(ctx),
                "smoothness_parent" => smoothness_parent(ctx),
                other => { tracing::warn!("unknown deriver: {other}"); None }
            },

            Producer::Extract { key, keys, from, side, sanitize, source, confidence } => {
                let tags = match from {
                    TagSet::Obj => Some(ctx.obj_tags),
                    TagSet::Parent => ctx.parent_tags, // strict: None when no parent
                    TagSet::ParentOrObj => Some(ctx.parent_tags.unwrap_or(ctx.obj_tags)),
                    TagSet::Centerline => Some(ctx.centerline_tags),
                }?;
                let raw = read_raw(tags, key.as_deref(), keys.as_deref(), side.as_deref())?;
                let value = match sanitize {
                    Some(name) => ctx.sanitizers.apply(name, raw)?,
                    None => Value::String(raw.to_owned()),
                };
                Some(Produced { value, source: source.clone(), confidence: confidence.clone() })
            }
        }
    }
}

/// Wrap a Rust-derived surface value with its provenance (`tag` own, `parent_highway_tag` copied).
fn surface_produced(value: Option<String>, from_parent: bool) -> Option<Produced> {
    value.map(|v| Produced {
        value: Value::String(v),
        source: Some(if from_parent { "parent_highway_tag" } else { "tag" }.to_owned()),
        confidence: Some("high".to_owned()),
    })
}

/// `deriveBikelaneSurface`: own surface, else (no own) the parent highway's surface.
fn surface_parent(ctx: &ExtractCtx) -> Option<Produced> {
    if let Some(own) = surface_produced(derive::surface(ctx.obj_tags, ctx.sanitizers), false) {
        return Some(own);
    }
    let parent = ctx.parent_tags?;
    surface_produced(derive::surface(parent, ctx.sanitizers), true)
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
    let own_from_tag = own.as_ref().map_or(false, |p| {
        matches!(p.source.as_deref(), Some("tag") | Some("tag_normalized"))
    });

    // A: own absent, and own surface absent or equal to the parent's.
    let cond_a = own.is_none() && (own_surface.is_none() || surfaces_match);
    // B: own not tag-sourced (derived or absent), own surface present and equal.
    let cond_b = !own_from_tag && own_surface.is_some() && surfaces_match;

    if cond_a || cond_b {
        par.map(|p| Produced {
            value: p.value,
            source: p.source.map(|s| format!("parent_highway_{s}")),
            confidence: p.confidence,
        })
    } else {
        own
    }
}

/// Resolve the raw string for an Extract: optional sided expansion, single `key`, or the
/// first present of `keys`.
fn read_raw<'a>(
    tags: &'a RawTags,
    key: Option<&str>,
    keys: Option<&[String]>,
    side: Option<&str>,
) -> Option<&'a str> {
    if let Some(side) = side {
        // sided lookup applies to the single key
        return sanitize::get_sided_with_bare_left(tags, key.expect("sided extract needs `key`"), side);
    }
    if let Some(key) = key {
        return tags.get(key).map(String::as_str);
    }
    if let Some(keys) = keys {
        return keys.iter().find_map(|k| tags.get(k).map(String::as_str));
    }
    None
}
