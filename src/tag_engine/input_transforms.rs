//! `InputTransform`: the runtime application of one in-place tag mutation, applied to an object's
//! tags before categorization. A generic `RawTags`-mutation primitive — `TagRule` wraps an
//! ordinary `Producer`; `StripPrefix`/`UnnestTags` are generic key-prefix operations. Nothing
//! here is topic-directory-specific; only the *construction* of a `Vec<InputTransform>` from
//! `topic.json`'s `input_transforms`/`split_sides` is (see `topic::runner::TopicRunner::load`).

use serde_json::Value;

use crate::tag_engine::producer::{ExtractCtx, Producer};
use crate::tag_engine::transform::side_split::SplitContext;
use crate::osm::types::RawTags;
use crate::value_sets::value_set;

/// One in-place tag mutation, applied to an object's tags before categorization — either at the
/// whole-way, pre-split stage (`obj_side: "self"`, no `parent_tags`), or, for `directed`-style
/// steps, per already-split object (its own resolved side + the parent way's tags). This is the
/// same primitive either way; only the `ExtractCtx` passed to `apply` differs.
#[derive(Clone)]
pub enum InputTransform {
    /// Write `output` from a full `Producer`. A produced `null` deletes `output`; a produced
    /// non-null value must be a string and overwrites it; no match (`None`) leaves it untouched.
    TagRule { output: String, source: Producer },
    /// Unnest bare `{prefix}[:{infix}]`-prefixed tags (plus each `meta_prefixes` entry's
    /// documentation companion, e.g. `source:`/`note:` — see
    /// `transform::side_split::unnest_prefixed_tags`) back onto the object's own tags, in place.
    /// `guard_value_set`, when set, only applies the unnest when the object's own `highway` value
    /// is a member of that named value set — this is what used to be the dedicated `SidepathSelf`
    /// variant (`guard_value_set: Some("sidepath_highway")`); a plain in-place unnest with no such
    /// convention just leaves it `None`.
    UnnestTags {
        prefix: &'static str,
        infix: &'static str,
        meta_prefixes: &'static [&'static str],
        guard_value_set: Option<&'static str>,
    },
    /// Strip `prefix` from matching keys — see `transform::strip_prefix`. The one step
    /// needing dynamic key iteration, so it isn't a `Producer`.
    StripPrefix {
        prefix: String,
        stamp_key: String,
        stamp_value: String,
        stamp_nested_under: Vec<String>,
    },
}

impl InputTransform {
    pub fn apply(
        &self,
        tags: &mut RawTags,
        parent_tags: Option<&RawTags>,
        obj_side: &'static str,
        prefix: Option<&'static str>,
        infix: Option<&'static str>,
    ) {
        match self {
            InputTransform::TagRule { output, source } => {
                let ctx = ExtractCtx {
                    obj_tags: tags,
                    parent_tags,
                    split: SplitContext { obj_side, prefix, infix },
                    id: "",
                };
                if let Some(p) = source.eval(&ctx) {
                    match p.value {
                        Value::Null => { tags.remove(output); }
                        Value::String(s) => { tags.insert(output.clone(), s); }
                        other => panic!(
                            "tag_rules for '{output}' produced a non-string, non-null value: {other}"
                        ),
                    }
                }
            }
            InputTransform::UnnestTags { prefix, infix, meta_prefixes, guard_value_set } => {
                if let Some(vs) = guard_value_set {
                    let highway = tags.get("highway").cloned().unwrap_or_default();
                    if !value_set(vs).contains(highway.as_str()) {
                        return;
                    }
                }
                let source = tags.clone();
                crate::tag_engine::transform::side_split::unnest_prefixed_tags(&source, prefix, infix, meta_prefixes, tags);
            }
            InputTransform::StripPrefix { prefix, stamp_key, stamp_value, stamp_nested_under } => {
                crate::tag_engine::transform::strip_prefix(tags, prefix, stamp_key, stamp_value, stamp_nested_under);
            }
        }
    }
}
