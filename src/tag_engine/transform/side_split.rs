use crate::osm::types::RawTags;
use crate::output::types::Side;
use crate::value_sets::value_set;

/// A way object after center-line splitting.
pub struct TransformedObject {
    pub side: Side,
    /// The prefix that produced this object, e.g. "cycleway". None for the self object.
    pub prefix: Option<&'static str>,
    /// The infix that matched: "" = bare prefix, "both", "left", or "right".
    /// None for the self object.
    pub infix: Option<&'static str>,
    /// Original highway value of the parent way (for left/right objects).
    pub parent_highway: Option<String>,
    /// The effective highway value for this object.
    pub highway: String,
    /// Flattened tags for this object (no `_`-prefixed internal keys). Only unnesting has run —
    /// `directed_keys`/`self_directed_keys` projection is a separate, later, per-object
    /// `InputTransform` pass (see `CenterLineTransformation::directed_steps` and its call site in
    /// `topic::pipeline`), not something the split itself does.
    pub tags: RawTags,
}

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

/// For ways whose own `highway` is a sidepath class (the `sidepath_highway` value set), unnest
/// bare `prefix`-prefixed tags (and `source:`/`note:` meta variants) onto the way itself. Models
/// the OSM convention of tagging a way's own cycling function directly on it (e.g. `highway=path`
/// + `cycleway=track`), as opposed to `split_sides` projecting side tags onto separate child
/// objects. Must run after `exclude_condition` and before `get_transformed_objects`, mirroring
/// where this used to run inline (see topic.json's `unnest_sidepath_self` transform).
pub fn apply_sidepath_self(tags: &mut RawTags, prefixes: &[&str]) {
    let highway = tags.get("highway").cloned().unwrap_or_default();
    if !value_set("sidepath_highway").contains(highway.as_str()) {
        return;
    }

    for prefix in prefixes {
        let source = tags.clone();
        unnest_prefixed_tags(&source, prefix, "", None, tags);
        for meta in META_PREFIXES {
            unnest_prefixed_tags(&source, prefix, "", Some(meta), tags);
        }
    }
}

/// Port of `GetTransformedObjects` from transformations.lua.
///
/// Returns an ordered list: [self, left?, right?, ...] for all transformations.
pub fn get_transformed_objects(
    tags: RawTags,
    transformations: &[CenterLineTransformation],
) -> Vec<TransformedObject> {
    let highway = tags.get("highway").cloned().unwrap_or_default();

    // Sidepath-class ways (see `apply_sidepath_self`) are never split into sides — any side
    // tagging they carry describes their own alignment, already folded onto this way's own tags.
    if value_set("sidepath_highway").contains(highway.as_str()) {
        return vec![TransformedObject {
            side: Side::Self_,
            prefix: None,
            infix: None,
            parent_highway: None,
            highway,
            tags,
        }];
    }

    // Build any side objects first (borrowing `tags`), then move `tags` into the self object
    // rather than cloning it — the common case (roads, non-split ways) then does zero clones.
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

            // Priority (lowest to highest): bare < both < side-specific. Apply in that order,
            // tracking the highest-priority infix that contributed any data.
            // Mirrors Lua: unnestPrefixedTags called with '', ':both', ':side' in order.
            let mut matched_infix: &'static str = "";
            for infix in ["", "both", side_str] {
                let before = obj.len();
                unnest_prefixed_tags(&tags, transformation.prefix, infix, None, &mut obj);
                if obj.len() > before {
                    matched_infix = infix;
                }
            }

            // Meta-prefixed tags (source:, note:) — processed after, overwrite.
            for meta in META_PREFIXES {
                for infix in ["", "both", side_str] {
                    unnest_prefixed_tags(&tags, transformation.prefix, infix, Some(meta), &mut obj);
                }
            }

            // Only emit an object if something was actually projected.
            if obj.is_empty() {
                continue;
            }

            // Inject the effective highway value into tags so category conditions can read it.
            // In Lua the transformed object is a full tag table including highway=cycleway.
            obj.insert("highway".into(), transformation.highway.to_owned());

            side_objects.push(TransformedObject {
                side,
                prefix: Some(transformation.prefix),
                infix: Some(matched_infix),
                parent_highway: Some(highway.clone()),
                highway: transformation.highway.to_owned(),
                tags: obj,
            });
        }
    }

    // Self object takes ownership of `tags` — no clone. Order stays [self, left?, right?, ...].
    let mut results = Vec::with_capacity(1 + side_objects.len());
    results.push(TransformedObject {
        side: Side::Self_,
        prefix: None,
        infix: None,
        parent_highway: None,
        highway,
        tags,
    });
    results.extend(side_objects);
    results
}

/// Unnest tags matching `{meta}{prefix}[:{infix}]` onto `dest`.
///
/// `meta` is an optional meta-prefix (e.g. `"source:"`, `"note:"`); pass `None` for the plain
/// tag. The destination key is the meta key (`"source"`) when a meta-prefix is given, else the
/// bare `prefix`.
///
/// Example (no meta, prefix="cycleway", infix="left"), full prefix "cycleway:left":
///   key == "cycleway:left"        → dest["cycleway"] = val
///   key == "cycleway:left:width"  → dest["width"]    = val
/// Example (meta="source:", same prefix/infix), full prefix "source:cycleway:left":
///   key == "source:cycleway:left"        → dest["source"]       = val
///   key == "source:cycleway:left:width"  → dest["source:width"] = val
pub(crate) fn unnest_prefixed_tags(
    tags: &RawTags,
    prefix: &str,
    infix: &str,
    meta: Option<&str>,
    dest: &mut RawTags,
) {
    let meta_prefix = meta.unwrap_or("");
    let full_prefix = if infix.is_empty() {
        format!("{meta_prefix}{prefix}")
    } else {
        format!("{meta_prefix}{prefix}:{infix}")
    };
    let meta_key = meta.map(|m| m.trim_end_matches(':'));

    for (key, val) in tags {
        if !key.starts_with(&full_prefix) {
            continue;
        }

        if key == &full_prefix {
            // Case 1: exact match → dest[meta_key or prefix] = val
            dest.insert(meta_key.unwrap_or(prefix).to_owned(), val.clone());
        } else if key.len() > full_prefix.len() && key.as_bytes()[full_prefix.len()] == b':' {
            // Case 2: sub-key → dest[(meta:)suffix] = val
            let suffix = &key[full_prefix.len() + 1..];

            // Validate: when infix is empty, the first component of suffix must not itself be a side.
            if infix.is_empty() {
                let first = suffix.split(':').next().unwrap_or("");
                if matches!(first, "left" | "right" | "both") {
                    continue;
                }
            }

            let dest_key = match meta_key {
                Some(mk) => format!("{mk}:{suffix}"),
                None => suffix.to_owned(),
            };
            dest.insert(dest_key, val.clone());
        }
    }
}
