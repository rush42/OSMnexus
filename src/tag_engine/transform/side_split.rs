use crate::osm::types::RawTags;
use crate::output::types::Side;
use crate::tag_engine::producer::ExtractCtx;
use crate::value_sets::value_set;

/// The side-split-specific slice of an `ExtractCtx`: which side/prefix/infix this object is.
/// `parent_tags` is *not* here — it's a plain `ExtractCtx` field, since it's meaningful (or not)
/// independent of side-splitting (e.g. a directed-key `InputTransform` sets it without any
/// prefix/infix). The one thing that ever varies this across a way is `get_transformed_objects`
/// deciding cardinality — tag-only `InputTransform`s never touch it. `TransformedObject::extract_ctx`
/// is the sole non-trivial constructor of a full `ExtractCtx`; elsewhere (the pre-split pass,
/// `eval_filter`) uses `SplitContext::default()`.
#[derive(Clone, Copy)]
pub struct SplitContext {
    pub obj_side: &'static str,
    pub prefix: Option<&'static str>,
    pub infix: Option<&'static str>,
}

impl Default for SplitContext {
    fn default() -> Self {
        SplitContext { obj_side: "self", prefix: None, infix: None }
    }
}

/// A way object after center-line splitting.
pub struct TransformedObject {
    pub side: Side,
    /// The prefix that produced this object, e.g. "cycleway". None for the self object.
    pub prefix: Option<&'static str>,
    /// The infix that matched: "" = bare prefix, "both", "left", or "right".
    /// None for the self object.
    pub infix: Option<&'static str>,
    /// Whether this object has a parent way (true for left/right objects). Just a flag — the
    /// parent's own tags (incl. its `highway`) are reachable through `ExtractCtx::parent_tags`
    /// once `extract_ctx` resolves it against the self object, so nothing here needs to duplicate
    /// the parent's `highway` value.
    pub has_parent: bool,
    /// The effective highway value for this object.
    pub highway: String,
    /// Flattened tags for this object (no `_`-prefixed internal keys). Only unnesting has run —
    /// `directed_keys`/`self_directed_keys` projection is a separate, later, per-object
    /// `InputTransform` pass (see `CenterLineTransformation::directed_steps` and its call site in
    /// `topic::pipeline`), not something the split itself does.
    pub tags: RawTags,
    /// This object's row id: `default_id` unchanged for the self object, or
    /// `"{default_id}/{prefix}/{side}"` for a side object (e.g. `"way/123/cycleway/left"`) —
    /// see `get_transformed_objects`'s `default_id` parameter.
    pub id: String,
}

impl TransformedObject {
    /// Build this object's full `ExtractCtx` — its own tags, `self_obj`'s tags as `parent_tags`
    /// when this object has a parent, and its side/prefix/infix. `self_obj` is always
    /// `&transformed[0]`, the self object; passing it explicitly avoids a self-referential borrow
    /// into the `Vec` `get_transformed_objects` returned it from — see `iter_with_ctx` for the
    /// usual way to get one of these per object without threading that reference by hand.
    pub fn extract_ctx<'a>(&'a self, self_obj: &'a TransformedObject) -> ExtractCtx<'a> {
        let obj_side = match self.side {
            Side::Left => "left",
            Side::Right => "right",
            Side::Self_ => "self",
        };
        ExtractCtx {
            obj_tags: &self.tags,
            parent_tags: self.has_parent.then_some(&self_obj.tags),
            split: SplitContext { obj_side, prefix: self.prefix, infix: self.infix },
            id: &self.id,
        }
    }
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
/// `default_id` is the element's own row id (e.g. `"way/123"`); the self object keeps it
/// unchanged, each side object gets `"{default_id}/{prefix}/{side}"` (e.g.
/// `"way/123/cycleway/left"`) — see `TransformedObject::id`.
///
/// Returns an ordered list: [self, left?, right?, ...] for all transformations.
pub fn get_transformed_objects(
    tags: RawTags,
    transformations: &[CenterLineTransformation],
    default_id: &str,
) -> Vec<TransformedObject> {
    let highway = tags.get("highway").cloned().unwrap_or_default();

    // Sidepath-class ways (see `apply_sidepath_self`) are never split into sides — any side
    // tagging they carry describes their own alignment, already folded onto this way's own tags.
    if value_set("sidepath_highway").contains(highway.as_str()) {
        return vec![TransformedObject {
            side: Side::Self_,
            prefix: None,
            infix: None,
            has_parent: false,
            highway,
            tags,
            id: default_id.to_owned(),
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
                has_parent: true,
                highway: transformation.highway.to_owned(),
                tags: obj,
                id: format!("{default_id}/{}/{side_str}", transformation.prefix),
            });
        }
    }

    // Self object takes ownership of `tags` — no clone. Order stays [self, left?, right?, ...].
    let mut results = Vec::with_capacity(1 + side_objects.len());
    results.push(TransformedObject {
        side: Side::Self_,
        prefix: None,
        infix: None,
        has_parent: false,
        highway,
        tags,
        id: default_id.to_owned(),
    });
    results.extend(side_objects);
    results
}

/// Turn every object `get_transformed_objects` returned into its `ExtractCtx` — the usual way to
/// consume that result: callers (`topic::pipeline`) iterate `ExtractCtx`s directly and never
/// construct one, or touch `TransformedObject`/`SplitContext`, by hand. `transformed[0]` (the self
/// object) supplies `parent_tags` to every side object.
pub fn iter_with_ctx(transformed: &[TransformedObject]) -> impl Iterator<Item = ExtractCtx<'_>> {
    let self_obj = &transformed[0];
    transformed.iter().map(move |obj| obj.extract_ctx(self_obj))
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
