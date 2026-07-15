use serde_json::{Map, Value};

use crate::osm::types::RawTags;
use crate::tag_engine::filter::{eval, Filter};
use crate::tag_engine::input_transforms::InputTransform;
use crate::tag_engine::producer::ExtractCtx;

pub(crate) const META_PREFIXES: &[&str] = &["source:", "note:"];

/// One step in a topic's transform pipeline: either an ordinary in-place `InputTransform`, or a
/// `Clone` that spawns an additional object alongside the current one. This is the generic
/// mechanism cardinality-changing transforms (side-splitting today, anything else needing it
/// tomorrow) are built from — nothing here is side/cycleway-specific; `topic::runner` is what
/// turns `split_sides`/`input_transforms` JSON into a `Vec<TransformStep>`.
#[derive(Clone)]
pub enum TransformStep {
    Transform(InputTransform),
    Clone(CloneStep),
}

/// Spawns one additional object: fresh (empty) tags, its own nested `steps`, with the pipeline's
/// current object available as `parent_tags`. Literal, not parameterized — e.g. a left/right split
/// is two `CloneStep`s, each with its own literal `annotate`/`id_suffix`, not one declaration
/// forking over a list of values.
#[derive(Clone)]
pub struct CloneStep {
    /// Only attempt the clone if this holds against the *current* object (its tags/annotations
    /// at this point in the pipeline) — e.g. "the way isn't already this side's target highway
    /// type". `None` = always attempt it.
    pub when: Option<Filter>,
    /// Literal annotations stamped on the clone before its own `steps` run (e.g. `_side: "left"`).
    pub annotate: Vec<(String, String)>,
    /// Appended to the parent's own row id: `"{parent_id}/{id_suffix}"` (e.g. `"cycleway/left"`
    /// → `"way/123/cycleway/left"`).
    pub id_suffix: String,
    /// Run against the clone's own (freshly empty) tags/annotations, with the current object's
    /// tags available as `parent_tags` — a `Drop` here (or the clone's own `when` above) is what
    /// decides whether the clone actually survives to be emitted.
    pub steps: Vec<InputTransform>,
}

/// Run `steps` against `tags`/`annotations` in place, appending any surviving `Clone`s to
/// `clones` as `(tags, annotations, id)` triples (owned, not yet turned into `ExtractCtx`s — that
/// happens once the whole pipeline is done, so a clone's `parent_tags` can borrow the final
/// `tags` without a self-referential `Vec<ExtractCtx>`). Returns `false` iff the object itself
/// was dropped (a top-level `Drop` step fired) — the caller should stop immediately and emit
/// nothing at all, not even the `clones` collected so far.
pub fn run_transform_steps(
    tags: &mut RawTags,
    annotations: &mut Map<String, Value>,
    steps: &[TransformStep],
    default_id: &str,
    clones: &mut Vec<(RawTags, Map<String, Value>, String)>,
) -> bool {
    for step in steps {
        match step {
            TransformStep::Transform(it) => {
                if !it.apply(tags, annotations, None) {
                    return false;
                }
            }
            TransformStep::Clone(spec) => {
                if let Some(when) = &spec.when {
                    let ctx = ExtractCtx { obj_tags: tags, parent_tags: None, id: "", annotations };
                    if !eval(when, &ctx) {
                        continue;
                    }
                }
                let mut clone_tags = RawTags::default();
                let mut clone_annotations = Map::new();
                for (k, v) in &spec.annotate {
                    clone_annotations.insert(k.clone(), Value::String(v.clone()));
                }
                let mut kept = true;
                for step in &spec.steps {
                    if !step.apply(&mut clone_tags, &mut clone_annotations, Some(tags)) {
                        kept = false;
                        break;
                    }
                }
                if kept {
                    clones.push((clone_tags, clone_annotations, format!("{default_id}/{}", spec.id_suffix)));
                }
            }
        }
    }
    true
}

/// Unnest tags matching `{prefix}[:{infix}]` onto `dest`, plus — for each `meta_prefixes` entry
/// (e.g. `"source:"`, `"note:"`) — the meta-tag documenting the same matched key, if present.
///
/// A meta tag's key is always exactly `{meta}{the raw key that just matched}` (`source:` +
/// `cycleway:left:width` = `source:cycleway:left:width`), so each meta companion is a single
/// `O(1)` point lookup keyed off the match already in hand, not a separate `O(|tags|)` rescan of
/// `tags` per meta prefix — `tags` is scanned exactly once regardless of how many meta prefixes
/// are given. The one behavioral consequence: a meta tag with no corresponding real tag present
/// (e.g. a stray `source:cycleway:left:width` with no `cycleway:left:width`) is not projected —
/// there's nothing real for it to document.
///
/// The destination key for a meta companion is the meta name alone (`"source"`) for an exact
/// match, or `"{meta name}:{suffix}"` (`"source:width"`) for a sub-key match — it stays attached
/// to the same object as the real value, not renested under it.
///
/// Example (prefix="cycleway", infix="left"), full prefix "cycleway:left":
///   key == "cycleway:left"        → dest["cycleway"] = val, + dest["source"] if source: sibling exists
///   key == "cycleway:left:width"  → dest["width"]    = val, + dest["source:width"] if source: sibling exists
pub(crate) fn unnest_prefixed_tags(
    tags: &RawTags,
    prefix: &str,
    infix: &str,
    meta_prefixes: &[&str],
    dest: &mut RawTags,
) {
    let full_prefix = if infix.is_empty() {
        prefix.to_owned()
    } else {
        format!("{prefix}:{infix}")
    };

    for (key, val) in tags {
        if !key.starts_with(&full_prefix) {
            continue;
        }

        // `suffix: None` = exact match (`key == full_prefix`); `Some(s)` = a `:`-separated
        // sub-key — drives both the plain dest key and each meta companion's dest key below.
        let suffix: Option<&str> = if key == &full_prefix {
            None
        } else if key.len() > full_prefix.len() && key.as_bytes()[full_prefix.len()] == b':' {
            let s = &key[full_prefix.len() + 1..];
            // Validate: when infix is empty, the first component of suffix must not itself be a side.
            if infix.is_empty() {
                let first = s.split(':').next().unwrap_or("");
                if matches!(first, "left" | "right" | "both") {
                    continue;
                }
            }
            Some(s)
        } else {
            continue;
        };

        dest.insert(suffix.unwrap_or(prefix).to_owned(), val.clone());

        for meta in meta_prefixes {
            let Some(meta_val) = tags.get(&format!("{meta}{key}")) else { continue };
            let meta_key = meta.trim_end_matches(':');
            let dest_key = match suffix {
                Some(s) => format!("{meta_key}:{s}"),
                None => meta_key.to_owned(),
            };
            dest.insert(dest_key, meta_val.clone());
        }
    }
}

#[cfg(test)]
mod unnest_prefixed_tags_tests {
    use super::*;

    fn tags(pairs: &[(&str, &str)]) -> RawTags {
        pairs.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect()
    }

    #[test]
    fn exact_and_subkey_match_without_meta() {
        let src = tags(&[("cycleway:left", "lane"), ("cycleway:left:width", "1.5")]);
        let mut dest = RawTags::default();
        unnest_prefixed_tags(&src, "cycleway", "left", &[], &mut dest);
        assert_eq!(dest.get("cycleway").map(String::as_str), Some("lane"));
        assert_eq!(dest.get("width").map(String::as_str), Some("1.5"));
    }

    #[test]
    fn meta_companion_projected_alongside_real_value() {
        let src = tags(&[
            ("cycleway:left", "lane"),
            ("source:cycleway:left", "survey"),
            ("cycleway:left:width", "1.5"),
            ("source:cycleway:left:width", "survey"),
        ]);
        let mut dest = RawTags::default();
        unnest_prefixed_tags(&src, "cycleway", "left", &["source:", "note:"], &mut dest);
        assert_eq!(dest.get("cycleway").map(String::as_str), Some("lane"));
        assert_eq!(dest.get("source").map(String::as_str), Some("survey"));
        assert_eq!(dest.get("width").map(String::as_str), Some("1.5"));
        assert_eq!(dest.get("source:width").map(String::as_str), Some("survey"));
        assert!(!dest.contains_key("note"));
    }

    #[test]
    fn orphaned_meta_tag_is_not_projected() {
        // No `cycleway:left` present — its `source:` companion has nothing real to document.
        let src = tags(&[("source:cycleway:left", "survey")]);
        let mut dest = RawTags::default();
        unnest_prefixed_tags(&src, "cycleway", "left", &["source:"], &mut dest);
        assert!(dest.is_empty());
    }

    #[test]
    fn bare_side_component_after_empty_infix_is_rejected() {
        // `cycleway:left` under a bare (infix="") scan must not be mistaken for a sub-key of `cycleway`.
        let src = tags(&[("cycleway:left", "lane")]);
        let mut dest = RawTags::default();
        unnest_prefixed_tags(&src, "cycleway", "", &[], &mut dest);
        assert!(dest.is_empty());
    }
}

#[cfg(test)]
mod run_transform_steps_tests {
    use super::*;
    use crate::tag_engine::extract::Extract;

    fn tags(pairs: &[(&str, &str)]) -> RawTags {
        pairs.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect()
    }

    fn annotation_str<'a>(m: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
        m.get(key).and_then(Value::as_str)
    }

    /// A left/right cycleway split, hand-built the same way `topic::runner` synthesizes one from
    /// `SplitSidesSpec` — one `TransformStep::Clone` per side, each running the same
    /// bare/`both`/side-specific `UnnestTags` priority chain, a `TagsEmpty` drop, and a literal
    /// `highway` injection.
    fn cycleway_split_steps() -> Vec<TransformStep> {
        ["left", "right"].into_iter().map(|side_str| {
            let steps: Vec<InputTransform> = ["", "both", side_str].into_iter().map(|infix| {
                InputTransform::UnnestTags {
                    prefix: "cycleway",
                    infix,
                    meta_prefixes: META_PREFIXES,
                    guard: None,
                    record_infix_as: Some("_infix"),
                }
            }).chain([
                InputTransform::Drop { when: Filter::TagsEmpty { tags_empty: true } },
                InputTransform::TagRule {
                    output: "highway".to_owned(),
                    source: crate::tag_engine::producer::Producer::Match {
                        rules: Vec::new(),
                        default: Some(Value::String("cycleway".to_owned())),
                        consts: Map::new(),
                    },
                },
            ]).collect();
            TransformStep::Clone(CloneStep {
                when: Some(Filter::Not { not: Box::new(Filter::TagEq {
                    extract: Extract::Value { key: "highway".to_owned() },
                    sanitize: None,
                    eq: "cycleway".to_owned(),
                }) }),
                annotate: vec![("_side".to_owned(), side_str.to_owned()), ("_prefix".to_owned(), "cycleway".to_owned())],
                id_suffix: format!("cycleway/{side_str}"),
                steps,
            })
        }).collect()
    }

    #[test]
    fn side_with_no_matching_tags_is_dropped() {
        let mut tags = tags(&[("highway", "primary")]);
        let mut annotations = Map::new();
        let mut clones = Vec::new();
        let kept = run_transform_steps(&mut tags, &mut annotations, &cycleway_split_steps(), "way/1", &mut clones);
        assert!(kept);
        assert!(clones.is_empty());
    }

    #[test]
    fn side_with_matching_tags_is_kept_with_correct_id_and_infix() {
        let mut tags = tags(&[("highway", "primary"), ("cycleway:right", "lane")]);
        let mut annotations = Map::new();
        let mut clones = Vec::new();
        run_transform_steps(&mut tags, &mut annotations, &cycleway_split_steps(), "way/1", &mut clones);
        assert_eq!(clones.len(), 1);
        let (clone_tags, clone_annotations, id) = &clones[0];
        assert_eq!(id, "way/1/cycleway/right");
        assert_eq!(clone_tags.get("highway").map(String::as_str), Some("cycleway"));
        assert_eq!(annotation_str(clone_annotations, "_side"), Some("right"));
        assert_eq!(annotation_str(clone_annotations, "_infix"), Some("right"));
    }

    #[test]
    fn both_infix_is_overridden_by_side_specific() {
        let mut tags = tags(&[
            ("highway", "primary"),
            ("cycleway:both", "lane"),
            ("cycleway:right:width", "2"),
        ]);
        let mut annotations = Map::new();
        let mut clones = Vec::new();
        run_transform_steps(&mut tags, &mut annotations, &cycleway_split_steps(), "way/1", &mut clones);
        let right = clones.iter().find(|(_, a, _)| annotation_str(a, "_side") == Some("right")).unwrap();
        assert_eq!(annotation_str(&right.1, "_infix"), Some("right"));
    }

    #[test]
    fn already_target_highway_type_is_not_split() {
        let mut tags = tags(&[("highway", "cycleway"), ("cycleway:right", "lane")]);
        let mut annotations = Map::new();
        let mut clones = Vec::new();
        run_transform_steps(&mut tags, &mut annotations, &cycleway_split_steps(), "way/1", &mut clones);
        assert!(clones.is_empty());
    }

    #[test]
    fn top_level_drop_stops_the_pipeline_and_emits_nothing() {
        let mut tags = tags(&[("highway", "primary"), ("cycleway:right", "lane")]);
        let mut annotations = Map::new();
        let mut clones = Vec::new();
        let mut steps = vec![TransformStep::Transform(InputTransform::Drop {
            when: Filter::TagsEmpty { tags_empty: false }, // never empty here -> always drops
        })];
        steps.extend(cycleway_split_steps());
        let kept = run_transform_steps(&mut tags, &mut annotations, &steps, "way/1", &mut clones);
        assert!(!kept);
        assert!(clones.is_empty());
    }
}
