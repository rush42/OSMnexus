use crate::osm::types::RawTags;
use crate::output::types::Side;
use crate::tag_engine::producer::ExtractCtx;
use crate::value_sets::value_set;

/// The side-split-specific slice of an `ExtractCtx`: which side/prefix/infix this object is.
/// `parent_tags` is *not* here — it's a plain `ExtractCtx` field, since it's meaningful (or not)
/// independent of side-splitting (e.g. a directed-key `InputTransform` sets it without any
/// prefix/infix). The one thing that ever varies this across a way is `generate_sides` deciding
/// cardinality — tag-only `InputTransform`s never touch it. `generate_sides` is the sole
/// non-trivial constructor of a full `ExtractCtx`; elsewhere (the pre-split pass, `eval_filter`)
/// uses `SplitContext::default()`.
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

impl SplitContext {
    /// `(key, value)` pairs describing this context, for a caller that wants to write it into a
    /// row's `private` map generically (e.g. `_{key}`) rather than naming each field by hand.
    /// `obj_side` is always present; `prefix`/`infix` only for a side object.
    pub fn iter(&self) -> impl Iterator<Item = (&'static str, &'static str)> {
        [("side", Some(self.obj_side)), ("prefix", self.prefix), ("infix", self.infix)]
            .into_iter()
            .filter_map(|(k, v)| v.map(|v| (k, v)))
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
/// objects. Must run after `exclude_condition` and before `generate_sides`, mirroring
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

    // Sidepath-class ways (see `apply_sidepath_self`) are never split into sides — any side
    // tagging they carry describes their own alignment, already folded onto this way's own tags.
    if value_set("sidepath_highway").contains(highway.as_str()) {
        f(ExtractCtx {
            obj_tags: &tags,
            parent_tags: None,
            split: SplitContext::default(),
            id: default_id,
        });
        return;
    }

    /// A side object's data before it becomes an `ExtractCtx` — held only long enough to run its
    /// `directed_steps` (which need the self object's tags, still owned by `generate_sides`) and
    /// then, once `tags` is next to it in scope, immediately be turned into one.
    struct SideObj {
        side: Side,
        prefix: &'static str,
        infix: &'static str,
        tags: RawTags,
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

            side_objects.push(SideObj {
                side,
                prefix: transformation.prefix,
                infix: matched_infix,
                tags: obj,
                id: format!("{default_id}/{}/{side_str}", transformation.prefix),
            });
        }
    }

    f(ExtractCtx {
        obj_tags: &tags,
        parent_tags: None,
        split: SplitContext::default(),
        id: default_id,
    });

    // Per-object post-split steps (`directed_keys`/`self_directed_keys`, ported from
    // `split_sides`'s config into ordinary `InputTransform`s): applied to each side object's own
    // tags, using the self object's tags as `parent_tags` — still pre-categorization (it can
    // influence which category a side object matches), just after cardinality is decided, since it
    // needs each object's resolved `side` to pick `:forward`/`:backward`. Folded in here (rather
    // than left to the caller) so no side-specific logic needs to live outside this module.
    for mut obj in side_objects {
        let obj_side = match obj.side {
            Side::Left => "left",
            Side::Right => "right",
            Side::Self_ => unreachable!("side objects are never Self_"),
        };
        if let Some(transformation) = transformations.iter().find(|t| t.prefix == obj.prefix) {
            for step in transformation.directed_steps {
                step.apply(&mut obj.tags, Some(&tags), obj_side, Some(obj.prefix), Some(obj.infix));
            }
        }

        f(ExtractCtx {
            obj_tags: &obj.tags,
            parent_tags: Some(&tags),
            split: SplitContext { obj_side, prefix: Some(obj.prefix), infix: Some(obj.infix) },
            id: &obj.id,
        });
    }
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
