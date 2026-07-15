use serde_json::{Map, Value};

use crate::osm::types::RawTags;
use crate::output::types::Side;
use crate::tag_engine::filter::Filter;
use crate::tag_engine::input_transforms::InputTransform;
use crate::tag_engine::producer::ExtractCtx;
use crate::value_sets::value_set;

/// Describes how a prefix (e.g. "cycleway") is split into side objects.
pub struct CenterLineTransformation {
    /// The highway value the resulting side object gets.
    pub highway: &'static str,
    /// The tag prefix to look for (e.g. "cycleway").
    pub prefix: &'static str,
    /// Ordinary `InputTransform`s (`directed_keys` as `TagSet::Parent`-sourced entries, then
    /// `self_directed_keys` as `TagSet::Obj`-sourced ones — see `topic::runner`'s `SplitSides`
    /// parsing), applied to each resulting side object's own tags — after the split decides
    /// cardinality, but still before that object is categorized.
    pub directed_steps: &'static [crate::tag_engine::input_transforms::InputTransform],
}

pub(crate) const META_PREFIXES: &[&str] = &["source:", "note:"];

/// Build a fresh annotations map stamped with `_side` (always) and `_prefix`/`_infix` (side
/// objects only) — the one place these three keys get written. There is no dedicated side-split
/// context type: `_side`/`_prefix`/`_infix` are ordinary `annotations` entries a `Filter`
/// (`Side`/`Prefix`/`Infix`) or the decision tree's sentinel-keyed branching reads just like any
/// tag, and they ride along into a row's public output unchanged (see `topic::pipeline::
/// build_topic_rows`, which seeds a row's `annotations` from `ExtractCtx::annotations`).
fn side_annotations(obj_side: &str, prefix: Option<&str>, infix: Option<&str>) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("_side".to_owned(), Value::String(obj_side.to_owned()));
    if let Some(p) = prefix {
        m.insert("_prefix".to_owned(), Value::String(p.to_owned()));
    }
    if let Some(i) = infix {
        m.insert("_infix".to_owned(), Value::String(i.to_owned()));
    }
    m
}

/// Port of `GetTransformedObjects` from transformations.lua (renamed `generate_sides`).
///
/// `default_id` is the element's own row id (e.g. `"way/123"`); the self object keeps it
/// unchanged, each side object gets `"{default_id}/{prefix}/{side}"` (e.g.
/// `"way/123/cycleway/left"`).
///
/// Calls `f` once per resulting object, in order: self, then left?/right? for each
/// transformation. Pushing `ExtractCtx`s through a callback (rather than collecting and
/// returning them) sidesteps the self-referential borrow a `Vec<ExtractCtx>` would need — every
/// side object's `parent_tags` borrows the self object's still-in-scope `tags`.
pub fn generate_sides(
    tags: RawTags,
    transformations: &[CenterLineTransformation],
    default_id: &str,
    mut f: impl FnMut(ExtractCtx),
) {
    let highway = tags.get("highway").cloned().unwrap_or_default();

    // Sidepath-class ways (see `InputTransform::UnnestTags`'s `guard`) are never split into sides — any side
    // tagging they carry describes their own alignment, already folded onto this way's own tags.
    if value_set("sidepath_highway").contains(highway.as_str()) {
        let self_annotations = side_annotations("self", None, None);
        f(ExtractCtx {
            obj_tags: &tags,
            parent_tags: None,
            id: default_id,
            annotations: &self_annotations,
        });
        return;
    }

    /// A side object's data before it becomes an `ExtractCtx` — held only long enough to run its
    /// `directed_steps` (which need the self object's tags, still owned by `generate_sides`) and
    /// then, once `tags` is next to it in scope, immediately be turned into one.
    struct SideObj {
        prefix: &'static str,
        tags: RawTags,
        annotations: Map<String, Value>,
        id: String,
    }

    let mut side_objects = Vec::new();
    for transformation in transformations {
        // Don't split if the way is already the target highway type.
        if highway == transformation.highway {
            continue;
        }

        for side in [Side::Left, Side::Right] {
            let side_str: &'static str = match side {
                Side::Left => "left",
                Side::Right => "right",
                Side::Self_ => unreachable!(),
            };

            let mut obj: RawTags = RawTags::default();
            let mut annotations = Map::new();

            // Priority (lowest to highest): bare < both < side-specific. Apply in that order,
            // tracking the highest-priority infix that contributed any data. Each pass is the
            // same `InputTransform::UnnestTags` any topic.json step uses. Mirrors Lua:
            // unnestPrefixedTags called with '', ':both', ':side' in order.
            let mut matched_infix: &'static str = "";
            for infix in ["", "both", side_str] {
                let step = InputTransform::UnnestTags {
                    prefix: transformation.prefix,
                    infix,
                    meta_prefixes: META_PREFIXES,
                    guard: None,
                };
                let before = obj.len();
                step.apply(&mut obj, &mut annotations, Some(&tags));
                if obj.len() > before {
                    matched_infix = infix;
                }
            }

            // Only keep an object if something was actually projected into it — a freshly-built
            // object starts empty, so "still empty" here means every unnest attempt above found
            // nothing, run before `highway` gets injected below (which would otherwise make every
            // side object non-empty regardless of whether anything real was ever unnested).
            let drop = InputTransform::Drop { when: Filter::TagsEmpty { tags_empty: true } };
            let keep = drop.apply(&mut obj, &mut annotations, Some(&tags));
            if !keep {
                continue;
            }

            // Inject the effective highway value into tags so category conditions can read it.
            // In Lua the transformed object is a full tag table including highway=cycleway.
            obj.insert("highway".into(), transformation.highway.to_owned());

            side_objects.push(SideObj {
                prefix: transformation.prefix,
                tags: obj,
                annotations: side_annotations(side_str, Some(transformation.prefix), Some(matched_infix)),
                id: format!("{default_id}/{}/{side_str}", transformation.prefix),
            });
        }
    }

    let self_annotations = side_annotations("self", None, None);
    f(ExtractCtx {
        obj_tags: &tags,
        parent_tags: None,
        id: default_id,
        annotations: &self_annotations,
    });

    // Per-object post-split steps (`directed_keys`/`self_directed_keys`, ported from
    // `split_sides`'s config into ordinary `InputTransform`s): applied to each side object's own
    // tags, using the self object's tags as `parent_tags` — still pre-categorization (it can
    // influence which category a side object matches), just after cardinality is decided, since it
    // needs each object's resolved `side` to pick `:forward`/`:backward`. Folded in here (rather
    // than left to the caller) so no side-specific logic needs to live outside this module.
    for mut obj in side_objects {
        if let Some(transformation) = transformations.iter().find(|t| t.prefix == obj.prefix) {
            for step in transformation.directed_steps {
                step.apply(&mut obj.tags, &mut obj.annotations, Some(&tags));
            }
        }

        f(ExtractCtx {
            obj_tags: &obj.tags,
            parent_tags: Some(&tags),
            id: &obj.id,
            annotations: &obj.annotations,
        });
    }
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
mod generate_sides_tests {
    use super::*;

    fn tags(pairs: &[(&str, &str)]) -> RawTags {
        pairs.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect()
    }

    fn cycleway_transformation() -> CenterLineTransformation {
        CenterLineTransformation { highway: "cycleway", prefix: "cycleway", directed_steps: &[] }
    }

    fn annotation_str<'a>(ctx: &'a ExtractCtx, key: &str) -> Option<&'a str> {
        ctx.annotations.get(key).and_then(Value::as_str)
    }

    #[test]
    fn side_with_no_matching_tags_is_dropped() {
        // No `cycleway:*` tags at all — neither side should produce an object; only "self" is kept.
        let way_tags = tags(&[("highway", "primary")]);
        let transformations = vec![cycleway_transformation()];
        let mut ids = Vec::new();
        generate_sides(way_tags, &transformations, "way/1", |ctx| ids.push(ctx.id.to_owned()));
        assert_eq!(ids, vec!["way/1"]);
    }

    #[test]
    fn side_with_matching_tags_is_kept_with_correct_infix() {
        let way_tags = tags(&[("highway", "primary"), ("cycleway:right", "lane")]);
        let transformations = vec![cycleway_transformation()];
        let mut seen: Vec<(String, String, Option<String>)> = Vec::new();
        generate_sides(way_tags, &transformations, "way/1", |ctx| {
            seen.push((
                ctx.id.to_owned(),
                annotation_str(&ctx, "_side").unwrap_or("self").to_owned(),
                annotation_str(&ctx, "_infix").map(str::to_owned),
            ));
        });
        assert_eq!(seen, vec![
            ("way/1".to_owned(), "self".to_owned(), None),
            ("way/1/cycleway/right".to_owned(), "right".to_owned(), Some("right".to_owned())),
        ]);
    }

    #[test]
    fn both_infix_is_overridden_by_side_specific() {
        // Priority bare < both < side-specific: `cycleway:both` alone should win as "both", but a
        // more specific `cycleway:right` on top of it should win instead.
        let way_tags = tags(&[
            ("highway", "primary"),
            ("cycleway:both", "lane"),
            ("cycleway:right:width", "2"),
        ]);
        let transformations = vec![cycleway_transformation()];
        let mut right_infix = None;
        generate_sides(way_tags, &transformations, "way/1", |ctx| {
            if annotation_str(&ctx, "_side") == Some("right") {
                right_infix = annotation_str(&ctx, "_infix").map(str::to_owned);
            }
        });
        assert_eq!(right_infix, Some("right".to_owned()));
    }
}
