//! `InputTransform`: the runtime application of one in-place tag mutation, applied to an object's
//! tags before categorization. A generic `RawTags`-mutation primitive — `TagRule` wraps an
//! ordinary `Producer`; `StripPrefix`/`UnnestTags` are generic key-prefix operations; `Drop` is
//! the one variant that isn't a mutation at all (see its own doc). Nothing here is
//! topic-directory-specific; only the *construction* of a `Vec<InputTransform>` from
//! `topic.json`'s `input_transforms`/`split_sides` is (see `topic::runner::TopicRunner::load`).

use serde_json::{Map, Value};

use crate::tag_engine::filter::{eval, Filter};
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
    /// `transform::side_split::unnest_prefixed_tags`) onto `tags`, in place (so `tags` doubles as
    /// both the source to scan and the destination — for a whole-way self-unnest that's the same
    /// object; for building a side object's tags from scratch, `tags` is that (initially empty)
    /// object, scanned against its own pre-populated content each call).
    /// `guard_value_set`, when set, only applies the unnest when the object's own `highway` value
    /// is a member of that named value set — this is what used to be the dedicated `SidepathSelf`
    /// variant (`guard_value_set: Some("sidepath_highway")`); a plain in-place unnest with no such
    /// convention just leaves it `None`.
    /// `mark`, when set, stamps `annotations[mark] = true` iff this call actually unnested
    /// something — the mechanism `Drop` reads to tell "nothing was ever unnested" apart from "the
    /// object legitimately has no other tags".
    UnnestTags {
        prefix: &'static str,
        infix: &'static str,
        meta_prefixes: &'static [&'static str],
        guard_value_set: Option<&'static str>,
        mark: Option<&'static str>,
    },
    /// Strip `prefix` from matching keys — see `transform::strip_prefix`. The one step
    /// needing dynamic key iteration, so it isn't a `Producer`.
    StripPrefix {
        prefix: String,
        stamp_key: String,
        stamp_value: String,
        stamp_nested_under: Vec<String>,
    },
    /// Remove this object from the active set when `when` holds — `apply`'s only variant that
    /// returns `false`. Every other variant is a pure tag mutation and always keeps the object;
    /// `Drop` carries no mutation of its own; it's the generic replacement for what used to be
    /// `generate_sides`' hand-rolled "skip this side object if nothing was ever unnested into it"
    /// check (`Drop { when: AnnotationExists { key: <UnnestTags's mark>, exists: false } }`).
    Drop { when: Filter },
}

impl InputTransform {
    /// Returns whether the object should be kept in the active set — always `true` except for
    /// `Drop`, which returns `false` exactly when its `when` filter holds.
    pub fn apply(
        &self,
        tags: &mut RawTags,
        annotations: &mut Map<String, Value>,
        parent_tags: Option<&RawTags>,
        obj_side: &'static str,
        prefix: Option<&'static str>,
        infix: Option<&'static str>,
    ) -> bool {
        match self {
            InputTransform::TagRule { output, source } => {
                let ctx = ExtractCtx {
                    obj_tags: tags,
                    parent_tags,
                    split: SplitContext { obj_side, prefix, infix },
                    id: "",
                    annotations,
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
                true
            }
            InputTransform::UnnestTags { prefix, infix, meta_prefixes, guard_value_set, mark } => {
                if let Some(vs) = guard_value_set {
                    let highway = tags.get("highway").cloned().unwrap_or_default();
                    if !value_set(vs).contains(highway.as_str()) {
                        return true;
                    }
                }
                let before = tags.len();
                match parent_tags {
                    // Cross-object unnest (e.g. `generate_sides` building a side object's tags
                    // from the way's own, `tags` starting empty): scan the given source, write
                    // into `tags`.
                    Some(source) => crate::tag_engine::transform::side_split::unnest_prefixed_tags(source, prefix, infix, meta_prefixes, tags),
                    // Self-unnest (e.g. `SidepathSelf`): scan-and-mutate the same object, so the
                    // scan needs its own snapshot to avoid borrowing `tags` both ways at once.
                    None => {
                        let source = tags.clone();
                        crate::tag_engine::transform::side_split::unnest_prefixed_tags(&source, prefix, infix, meta_prefixes, tags);
                    }
                }
                if let Some(mark) = mark {
                    if tags.len() > before {
                        annotations.insert((*mark).to_owned(), Value::Bool(true));
                    }
                }
                true
            }
            InputTransform::StripPrefix { prefix, stamp_key, stamp_value, stamp_nested_under } => {
                crate::tag_engine::transform::strip_prefix(tags, prefix, stamp_key, stamp_value, stamp_nested_under);
                true
            }
            InputTransform::Drop { when } => {
                let ctx = ExtractCtx {
                    obj_tags: tags,
                    parent_tags,
                    split: SplitContext { obj_side, prefix, infix },
                    id: "",
                    annotations,
                };
                !eval(when, &ctx)
            }
        }
    }
}

#[cfg(test)]
mod unnest_tags_tests {
    use super::*;

    fn tags(pairs: &[(&str, &str)]) -> RawTags {
        pairs.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect()
    }

    #[test]
    fn self_unnest_scans_and_mutates_the_same_object() {
        let mut obj = tags(&[("highway", "path"), ("cycleway", "track"), ("cycleway:width", "1.5")]);
        let mut annotations = Map::new();
        let step = InputTransform::UnnestTags {
            prefix: "cycleway", infix: "", meta_prefixes: &[], guard_value_set: None, mark: Some("_unnested"),
        };
        let kept = step.apply(&mut obj, &mut annotations, None, "self", None, None);
        assert!(kept);
        assert_eq!(obj.get("width").map(String::as_str), Some("1.5"));
        assert_eq!(annotations.get("_unnested"), Some(&Value::Bool(true)));
    }

    #[test]
    fn cross_object_unnest_scans_parent_tags_writes_into_tags() {
        let way_tags = tags(&[("highway", "primary"), ("cycleway:right", "lane")]);
        let mut obj = RawTags::default();
        let mut annotations = Map::new();
        let step = InputTransform::UnnestTags {
            prefix: "cycleway", infix: "right", meta_prefixes: &[], guard_value_set: None, mark: Some("_unnested"),
        };
        step.apply(&mut obj, &mut annotations, Some(&way_tags), "right", Some("cycleway"), Some("right"));
        assert_eq!(obj.get("cycleway").map(String::as_str), Some("lane"));
        assert_eq!(annotations.get("_unnested"), Some(&Value::Bool(true)));
    }

    #[test]
    fn guard_value_set_blocks_unrelated_highway() {
        let mut obj = tags(&[("highway", "primary"), ("cycleway", "track")]);
        let mut annotations = Map::new();
        let step = InputTransform::UnnestTags {
            prefix: "cycleway", infix: "", meta_prefixes: &[], guard_value_set: Some("sidepath_highway"), mark: None,
        };
        // "primary" is never a sidepath_highway value in any topic's value_sets.json.
        step.apply(&mut obj, &mut annotations, None, "self", None, None);
        assert!(!obj.contains_key("width"));
    }

    #[test]
    fn drop_removes_object_when_filter_holds() {
        let mut obj = RawTags::default();
        let mut annotations = Map::new();
        let drop = InputTransform::Drop {
            when: Filter::AnnotationExists { key: "_unnested".to_owned(), exists: false },
        };
        assert!(!drop.apply(&mut obj, &mut annotations, None, "right", None, None));

        annotations.insert("_unnested".to_owned(), Value::Bool(true));
        assert!(drop.apply(&mut obj, &mut annotations, None, "right", None, None));
    }
}
