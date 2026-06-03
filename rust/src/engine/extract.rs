//! The extraction layer: how a field's value is produced from a way's tags.
//!
//! A `Producer` is either an `Extract` (read a tag — single `key` or first-present `keys`,
//! from obj/parent/centerline, optional `side` expansion, optional `sanitize`), a `fallback`
//! over producers (first non-empty wins), or a `derive` call. This one combinator subsumes
//! the old obj-then-parent lookup, multi-key lookup, `get_sided_with_bare_left`, and the
//! `surface:colour`/`surface:color` fallback.

use serde::Deserialize;
use serde_json::Value;

use crate::classify::{derive, sanitize};
use crate::osm::types::RawTags;

/// Everything a producer might need to resolve a value.
pub struct ExtractCtx<'a> {
    pub obj_tags: &'a RawTags,
    pub parent_tags: Option<&'a RawTags>,
    pub centerline_tags: &'a RawTags,
    pub implicit_oneway: bool,
    /// Matched category id and the transformed object's side — used by the `traffic_mode` deriver.
    pub category_id: &'a str,
    pub obj_side: &'a str,
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
    /// A Rust-backed deriver. `out_side` fixes the side for the per-side `traffic_mode` deriver.
    Derive { derive: String, #[serde(default)] out_side: Option<String> },
    Extract {
        #[serde(default)] key: Option<String>,
        #[serde(default)] keys: Option<Vec<String>>,
        #[serde(default)] from: TagSet,
        #[serde(default)] side: Option<String>,
        #[serde(default)] sanitize: Option<String>,
    },
}

impl Producer {
    pub fn eval(&self, ctx: &ExtractCtx) -> Option<Value> {
        match self {
            Producer::Fallback { fallback } => fallback.iter().find_map(|p| p.eval(ctx)),

            Producer::Derive { derive, out_side } => match derive.as_str() {
                "oneway" => Some(Value::String(
                    derive::derive_oneway(ctx.obj_tags, ctx.implicit_oneway),
                )),
                "traffic_mode" => {
                    let out_side = out_side.as_deref()
                        .expect("traffic_mode deriver needs `out_side`");
                    derive::traffic_mode_side(
                        ctx.obj_tags, ctx.centerline_tags, ctx.category_id, ctx.obj_side, out_side,
                    ).map(Value::String)
                }
                other => { tracing::warn!("unknown deriver: {other}"); None }
            },

            Producer::Extract { key, keys, from, side, sanitize } => {
                let tags = match from {
                    TagSet::Obj => Some(ctx.obj_tags),
                    TagSet::Parent => ctx.parent_tags, // strict: None when no parent
                    TagSet::ParentOrObj => Some(ctx.parent_tags.unwrap_or(ctx.obj_tags)),
                    TagSet::Centerline => Some(ctx.centerline_tags),
                }?;
                let raw = read_raw(tags, key.as_deref(), keys.as_deref(), side.as_deref())?;
                match sanitize {
                    Some(name) => sanitize::apply_sanitizer(name, raw),
                    None => Some(Value::String(raw.to_owned())),
                }
            }
        }
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
