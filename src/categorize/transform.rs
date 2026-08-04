//! Everything to do with a topic's transform pipeline: `InputTransform` (one in-place tag
//! mutation), `TransformStep`/`CloneStep` (the object-cardinality-changing wrapper around it —
//! side-splitting today, anything else needing it tomorrow), and the two dynamic-key-iteration
//! helpers (`unnest_prefixed_tags`, `strip_prefix`) `InputTransform`'s own variants are built
//! from. One file because these are really one concept at different granularities — a single
//! step, a pipeline of them, and the couple of primitives no bare `Producer` output can express
//! (dynamic key iteration) — not separate subsystems. Nothing here is topic-directory-specific;
//! only the *construction* of a `Vec<TransformStep>` from `topic.json`'s `input_transforms`/
//! `split_sides` is (see `topic::runner::TopicRunner::load`).

use serde::Deserialize;
use serde_json::{Map, Value};

use crate::lang::extract::first_present;
use crate::lang::filter::{eval, Filter};
use crate::lang::producer::{ExtractCtx, Producer};
use crate::lang::sanitize::{eval_sanitize, Sanitizer};
use crate::osm::types::RawTags;

// ── InputTransform ──────────────────────────────────────────────────────────────

/// One in-place tag mutation, applied to an object's tags before categorization — either at the
/// whole-way, pre-split stage (no `parent_tags`), or, for `directed`-style steps, per already-split
/// object (its own annotated side + the parent way's tags). This is the same primitive either way;
/// only the `ExtractCtx` passed to `apply` differs.
#[derive(Clone)]
pub enum InputTransform {
    /// Write `output` from a full `Producer`. A produced `null` deletes `output`; a produced
    /// non-null value must be a string and overwrites it; no match (`None`) leaves it untouched.
    TagRule { output: String, source: Producer },
    /// Unnest bare `{prefix}[:{infix}]`-prefixed tags (plus each `meta_prefixes` entry's
    /// documentation companion, e.g. `source:`/`note:` — see `unnest_prefixed_tags`) onto `tags`,
    /// in place (so `tags` doubles as both the source to scan and the destination — for a
    /// whole-way self-unnest that's the same object; for building a side object's tags from
    /// scratch, `tags` is that (initially empty) object, scanned against its own pre-populated
    /// content each call).
    /// `guard`, when set, only applies the unnest when it holds — this is what used to be the
    /// dedicated `SidepathSelf` variant (`guard: Some(TagInSet{tag: "highway", in_set:
    /// "sidepath_highway"})`); a plain in-place unnest with no such condition just leaves it
    /// `None`. Evaluated against the same context `Drop`'s own condition sees (`tags` as they
    /// stand before this call, `parent_tags`, `annotations`).
    /// `record_infix_as`, when set, stamps `annotations[key] = infix` iff this call actually
    /// unnested something — the mechanism for tracking *which* of several priority-ordered
    /// attempts (bare < both < side-specific) actually won, since a plain `TagsEmpty` check only
    /// answers *whether* any of them did.
    UnnestTags {
        prefix: &'static str,
        infix: &'static str,
        meta_prefixes: &'static [&'static str],
        guard: Option<Filter>,
        record_infix_as: Option<&'static str>,
    },
    /// Strip `prefix` from matching keys — see `strip_prefix`. The one step needing dynamic key
    /// iteration, so it isn't a `Producer`.
    StripPrefix {
        prefix: String,
        stamp_key: String,
        stamp_value: String,
        stamp_nested_under: Vec<String>,
    },
    /// Direction-sensitive read of `key`, written onto `output` — resolves `key`'s
    /// `:forward`/`:backward` variant from `annotations["_side"]` + the global
    /// left/right-hand-traffic setting (`traffic::is_left_hand_traffic`), producing nothing for a
    /// `self` object (no direction to resolve). Every real use is exactly this shape: a step
    /// placed right after the unnest steps that built this (already-split) object, so `apply`'s
    /// own `parent_tags` is all the dual-tagset access it needs — no separate `ExtractCtx`-per-read
    /// plumbing the way a live `Filter`/`Match` read would require.
    DirectedExtract {
        output: String,
        key: String,
        from: DirectedFrom,
        sanitize: Vec<Sanitizer>,
    },
    /// Remove this object from the active set when `when` holds — `apply`'s only variant that
    /// returns `false`. Every other variant is a pure tag mutation and always keeps the object;
    /// `Drop` carries no mutation of its own. A freshly-cloned object starts empty, so
    /// `Drop { when: TagsEmpty { tags_empty: true } }` run right after the unnest steps (and
    /// before any literal-value injection, e.g. `highway`) is "skip this clone if nothing was
    /// ever unnested into it", expressed through the same `Filter` engine as everything else —
    /// see `CloneStep`.
    Drop { when: Filter },
}

/// Which tagset `InputTransform::DirectedExtract` resolves its directed key against — its own,
/// narrower vocabulary, distinct from the general `TagSet` (`producer`): a directed read needs
/// both `parent_tags` and the object's own `obj_tags` simultaneously (parent tags need the
/// two-key, bare-then-directed fallback), so it can't be expressed as a plain "swap `obj_tags`,
/// recurse" wrapper the way every other tagset-scoping need is. No `ParentOrObj`: unlike the
/// general case, a directed read has nothing distinct to commit to — a `ParentOrObj` here could
/// only ever mean "try that, then plain `Obj`," which nobody's asked for; keeping it out of the
/// type means it can't be spelled by accident and silently behave like `Obj`.
#[derive(Debug, Deserialize, Clone, Copy, Default)]
#[serde(rename_all = "snake_case")]
pub enum DirectedFrom {
    #[default]
    Obj,
    Parent,
}

impl InputTransform {
    /// Returns whether the object should be kept in the active set — always `true` except for
    /// `Drop`, which returns `false` exactly when its `when` filter holds.
    pub fn apply<'a>(
        &self,
        tags: &mut RawTags<'a>,
        annotations: &mut Map<String, Value>,
        parent_tags: Option<&RawTags<'a>>,
    ) -> bool {
        match self {
            InputTransform::TagRule { output, source } => {
                let ctx = ExtractCtx { obj_tags: tags, parent_tags, id: "", annotations };
                if let Some(p) = source.eval(&ctx) {
                    match p.value {
                        Value::Null => { tags.remove(output.as_str()); }
                        Value::String(s) => { tags.insert(output.clone().into(), s.into()); }
                        other => panic!(
                            "tag_rules for '{output}' produced a non-string, non-null value: {other}"
                        ),
                    }
                }
                true
            }
            InputTransform::UnnestTags { prefix, infix, meta_prefixes, guard, record_infix_as } => {
                if let Some(guard) = guard {
                    let ctx = ExtractCtx { obj_tags: tags, parent_tags, id: "", annotations };
                    if !eval(guard, &ctx) {
                        return true;
                    }
                }
                let before = tags.len();
                match parent_tags {
                    // Cross-object unnest (e.g. a `Clone`'s own steps building a side object's
                    // tags from the way's own, `tags` starting empty): scan the given source,
                    // write into `tags`.
                    Some(source) => unnest_prefixed_tags(source, prefix, infix, meta_prefixes, tags),
                    // Self-unnest (e.g. `SidepathSelf`): scan-and-mutate the same object, so the
                    // scan needs its own snapshot to avoid borrowing `tags` both ways at once. Only
                    // the entries `unnest_prefixed_tags` could possibly touch — keys under `prefix`
                    // plus their meta companions — need to be in that snapshot, not the whole map
                    // (a way's tags routinely outnumber matches here by an order of magnitude).
                    None => {
                        let source: RawTags = tags.iter()
                            .filter(|(k, _)| k.starts_with(*prefix) || meta_prefixes.iter().any(|m| k.starts_with(*m)))
                            .map(|(k, v)| (k.clone(), v.clone()))
                            .collect();
                        unnest_prefixed_tags(&source, prefix, infix, meta_prefixes, tags);
                    }
                }
                if let Some(key) = record_infix_as {
                    if tags.len() > before {
                        annotations.insert((*key).to_owned(), Value::String((*infix).to_owned()));
                    }
                }
                true
            }
            InputTransform::DirectedExtract { output, key, from, sanitize } => {
                if let Some(raw) = read_directed(tags, parent_tags, annotations, key, *from) {
                    match eval_sanitize(sanitize, raw) {
                        None | Some(Value::Null) => {}
                        Some(Value::String(s)) => { tags.insert(output.clone().into(), s.into()); }
                        Some(other) => panic!(
                            "directed extract for '{output}' produced a non-string, non-null value: {other}"
                        ),
                    }
                }
                true
            }
            InputTransform::StripPrefix { prefix, stamp_key, stamp_value, stamp_nested_under } => {
                strip_prefix(tags, prefix, stamp_key, stamp_value, stamp_nested_under);
                true
            }
            InputTransform::Drop { when } => {
                let ctx = ExtractCtx { obj_tags: tags, parent_tags, id: "", annotations };
                !eval(when, &ctx)
            }
        }
    }
}

// ── TransformStep / CloneStep ───────────────────────────────────────────────────

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
pub fn run_transform_steps<'a>(
    tags: &mut RawTags<'a>,
    annotations: &mut Map<String, Value>,
    steps: &[TransformStep],
    default_id: &str,
    clones: &mut Vec<(RawTags<'a>, Map<String, Value>, String)>,
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

/// The read strategy behind `InputTransform::DirectedExtract` — needs both `tags` (to guard
/// against overriding an already-set key) and, when `from: Parent`, `parent_tags` (tried
/// bare-key-then-directed-key) at once, which is why it's a step in the transform pipeline rather
/// than a generic `Extract` shape: `apply`'s signature already carries both.
fn read_directed<'a>(
    tags: &'a RawTags<'a>,
    parent_tags: Option<&'a RawTags<'a>>,
    annotations: &Map<String, Value>,
    key: &str,
    from: DirectedFrom,
) -> Option<&'a str> {
    if tags.contains_key(key) {
        return None; // already set (e.g. by an earlier unnest) — don't override it
    }
    let obj_side = annotations.get("_side").and_then(Value::as_str).unwrap_or("self");
    let suffix = match (obj_side, crate::traffic::is_left_hand_traffic()) {
        ("left", false) | ("right", true) => ":backward",
        ("right", false) | ("left", true) => ":forward",
        _ => return None, // "self": no direction to resolve
    };
    let directed_key = format!("{key}{suffix}");
    match from {
        DirectedFrom::Parent => {
            let parent = parent_tags?;
            first_present(parent, [key, directed_key.as_str()])
        }
        DirectedFrom::Obj => first_present(tags, [directed_key.as_str()]),
    }
}

// ── Dynamic-key-iteration helpers ───────────────────────────────────────────────

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
pub(crate) fn unnest_prefixed_tags<'a>(
    tags: &RawTags<'a>,
    prefix: &str,
    infix: &str,
    meta_prefixes: &[&str],
    dest: &mut RawTags<'a>,
) {
    let full_prefix = if infix.is_empty() {
        prefix.to_owned()
    } else {
        format!("{prefix}:{infix}")
    };

    // Reused across meta-companion lookups below instead of a fresh `format!` allocation per
    // (matched key × meta prefix) pair — cleared and rewritten in place each time.
    let mut meta_key_buf = String::new();

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

        dest.insert(suffix.unwrap_or(prefix).to_owned().into(), val.clone());

        for meta in meta_prefixes {
            meta_key_buf.clear();
            meta_key_buf.push_str(meta);
            meta_key_buf.push_str(key);
            let Some(meta_val) = tags.get(meta_key_buf.as_str()) else { continue };
            let meta_key = meta.trim_end_matches(':');
            let dest_key = match suffix {
                Some(s) => format!("{meta_key}:{s}"),
                None => meta_key.to_owned(),
            };
            dest.insert(dest_key.into(), meta_val.clone());
        }
    }
}

/// For every key starting with `prefix`, strip it, re-key the value onto the base tag, and stamp
/// a marker. The marker key is `<base>:<stamp_key>` when the base starts with one of
/// `stamp_nested_under`, else `stamp_key`. The one remaining native in-place tag transform: it
/// needs to iterate keys matching a runtime-unknown pattern, which no `Producer`/`Rule` primitive
/// can express (they all name their target key(s) statically). Everything else that used to live
/// here (`lifecycle`, `rename_key`, `value_cases`) is expressible as `tag_rules` `Producer`
/// entries and has moved to topic JSON.
pub fn strip_prefix<'a>(
    tags: &mut RawTags<'a>,
    prefix: &str,
    stamp_key: &str,
    stamp_value: &str,
    stamp_nested_under: &[String],
) {
    // Only the keys need cloning here (can't mutate `tags` while iterating it) — each matched
    // value is moved out via `remove` below instead of being cloned up front.
    let matched: Vec<std::borrow::Cow<'a, str>> = tags
        .iter()
        .filter(|(k, _)| k.starts_with(prefix))
        .map(|(k, _)| k.clone())
        .collect();

    for key in matched {
        let value = tags.remove(key.as_ref()).expect("key just collected from tags");
        let base = key.as_ref()[prefix.len()..].to_owned();
        tags.insert(base.clone().into(), value);

        let marker = if stamp_nested_under.iter().any(|p| base.starts_with(p.as_str())) {
            format!("{base}:{stamp_key}")
        } else {
            stamp_key.to_owned()
        };
        tags.insert(marker.into(), stamp_value.to_owned().into());
    }
}

#[cfg(test)]
mod unnest_tags_tests {
    use super::*;

    fn tags<'a>(pairs: &[(&'a str, &'a str)]) -> RawTags<'a> {
        pairs.iter().map(|&(k, v)| (std::borrow::Cow::Borrowed(k), std::borrow::Cow::Borrowed(v))).collect()
    }

    #[test]
    fn self_unnest_scans_and_mutates_the_same_object() {
        let mut obj = tags(&[("highway", "path"), ("cycleway", "track"), ("cycleway:width", "1.5")]);
        let mut annotations = Map::new();
        let step = InputTransform::UnnestTags {
            prefix: "cycleway", infix: "", meta_prefixes: &[], guard: None, record_infix_as: None,
        };
        let kept = step.apply(&mut obj, &mut annotations, None);
        assert!(kept);
        assert_eq!(obj.get("width").map(|v| v.as_ref()), Some("1.5"));
    }

    #[test]
    fn cross_object_unnest_scans_parent_tags_writes_into_tags() {
        let way_tags = tags(&[("highway", "primary"), ("cycleway:right", "lane")]);
        let mut obj = RawTags::default();
        let mut annotations = Map::new();
        let step = InputTransform::UnnestTags {
            prefix: "cycleway", infix: "right", meta_prefixes: &[], guard: None, record_infix_as: None,
        };
        step.apply(&mut obj, &mut annotations, Some(&way_tags));
        assert_eq!(obj.get("cycleway").map(|v| v.as_ref()), Some("lane"));
    }

    #[test]
    fn guard_blocks_unrelated_highway() {
        let mut obj = tags(&[("highway", "primary"), ("cycleway", "track")]);
        let mut annotations = Map::new();
        let step = InputTransform::UnnestTags {
            prefix: "cycleway", infix: "", meta_prefixes: &[],
            guard: Some(Filter::InSet {
                extract: crate::lang::extract::Extract::Value { key: "highway".to_owned(), sanitize: vec![] },
                in_set: "sidepath_highway".to_owned(),
            }),
            record_infix_as: None,
        };
        // "primary" is never a sidepath_highway value in any topic's value_sets.json.
        step.apply(&mut obj, &mut annotations, None);
        assert!(!obj.contains_key("width"));
    }

    #[test]
    fn record_infix_as_stamps_only_on_success_and_last_wins() {
        let way_tags = tags(&[("cycleway:both", "lane"), ("cycleway:right:width", "2")]);
        let mut obj = RawTags::default();
        let mut annotations = Map::new();
        for infix in ["", "both", "right"] {
            let step = InputTransform::UnnestTags {
                prefix: "cycleway", infix, meta_prefixes: &[], guard: None, record_infix_as: Some("_infix"),
            };
            step.apply(&mut obj, &mut annotations, Some(&way_tags));
        }
        // Priority bare < both < side-specific: "both" matches (stamping "both"), then "right"
        // also matches (its subkey `width`), overwriting the recorded infix to "right".
        assert_eq!(annotations.get("_infix").and_then(Value::as_str), Some("right"));
    }

    #[test]
    fn drop_removes_object_when_tags_are_empty() {
        let mut obj = RawTags::default();
        let mut annotations = Map::new();
        let drop = InputTransform::Drop { when: Filter::TagsEmpty { tags_empty: true } };
        assert!(!drop.apply(&mut obj, &mut annotations, None));

        obj.insert("cycleway".into(), "lane".into());
        assert!(drop.apply(&mut obj, &mut annotations, None));
    }
}

#[cfg(test)]
mod directed_extract_tests {
    use super::*;

    fn tags<'a>(pairs: &[(&'a str, &'a str)]) -> RawTags<'a> {
        pairs.iter().map(|&(k, v)| (std::borrow::Cow::Borrowed(k), std::borrow::Cow::Borrowed(v))).collect()
    }

    fn side_annotations(side: &str) -> Map<String, Value> {
        let mut m = Map::new();
        m.insert("_side".to_owned(), Value::String(side.to_owned()));
        m
    }

    fn step(key: &str, from: DirectedFrom) -> InputTransform {
        InputTransform::DirectedExtract { output: key.to_owned(), key: key.to_owned(), from, sanitize: Vec::new() }
    }

    #[test]
    fn parent_source_prefers_existing_obj_value() {
        let mut obj = tags(&[("cycleway:lanes", "existing")]);
        let parent = tags(&[("cycleway:lanes:forward", "lane")]);
        let mut annotations = side_annotations("right");
        step("cycleway:lanes", DirectedFrom::Parent).apply(&mut obj, &mut annotations, Some(&parent));
        assert_eq!(obj.get("cycleway:lanes").map(|v| v.as_ref()), Some("existing"));
    }

    #[test]
    fn parent_source_falls_back_to_bare_then_directed_key() {
        let mut obj = RawTags::default();
        let parent = tags(&[("cycleway:lanes:forward", "lane")]);
        let mut annotations = side_annotations("right");
        step("cycleway:lanes", DirectedFrom::Parent).apply(&mut obj, &mut annotations, Some(&parent));
        assert_eq!(obj.get("cycleway:lanes").map(|v| v.as_ref()), Some("lane"));

        let mut obj = RawTags::default();
        let parent = RawTags::default();
        step("cycleway:lanes", DirectedFrom::Parent).apply(&mut obj, &mut annotations, Some(&parent));
        assert!(!obj.contains_key("cycleway:lanes"));
    }

    #[test]
    fn self_source_reads_from_obj_own_directed_key() {
        let mut obj = tags(&[("traffic_sign:forward", "DE:1022-10")]);
        let mut annotations = side_annotations("right");
        step("traffic_sign", DirectedFrom::Obj).apply(&mut obj, &mut annotations, None);
        assert_eq!(obj.get("traffic_sign").map(|v| v.as_ref()), Some("DE:1022-10"));
    }

    #[test]
    fn noop_for_self_side() {
        let mut obj = RawTags::default();
        let parent = tags(&[("cycleway:lanes:forward", "lane")]);
        let mut annotations = side_annotations("self");
        step("cycleway:lanes", DirectedFrom::Parent).apply(&mut obj, &mut annotations, Some(&parent));
        assert!(!obj.contains_key("cycleway:lanes"));
    }

    #[test]
    fn handedness_flips_suffix() {
        let parent = tags(&[("cycleway:lanes:backward", "lane")]);

        // Right-hand traffic (global default in tests): Side::Right reads `:forward`, not
        // `:backward` — so this should NOT match.
        let mut obj = RawTags::default();
        let mut right = side_annotations("right");
        step("cycleway:lanes", DirectedFrom::Parent).apply(&mut obj, &mut right, Some(&parent));
        assert!(!obj.contains_key("cycleway:lanes"));

        let mut obj = RawTags::default();
        let mut left = side_annotations("left");
        step("cycleway:lanes", DirectedFrom::Parent).apply(&mut obj, &mut left, Some(&parent));
        assert_eq!(obj.get("cycleway:lanes").map(|v| v.as_ref()), Some("lane"));
    }
}

#[cfg(test)]
mod unnest_prefixed_tags_tests {
    use super::*;

    fn tags<'a>(pairs: &[(&'a str, &'a str)]) -> RawTags<'a> {
        pairs.iter().map(|&(k, v)| (std::borrow::Cow::Borrowed(k), std::borrow::Cow::Borrowed(v))).collect()
    }

    #[test]
    fn exact_and_subkey_match_without_meta() {
        let src = tags(&[("cycleway:left", "lane"), ("cycleway:left:width", "1.5")]);
        let mut dest = RawTags::default();
        unnest_prefixed_tags(&src, "cycleway", "left", &[], &mut dest);
        assert_eq!(dest.get("cycleway").map(|v| v.as_ref()), Some("lane"));
        assert_eq!(dest.get("width").map(|v| v.as_ref()), Some("1.5"));
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
        assert_eq!(dest.get("cycleway").map(|v| v.as_ref()), Some("lane"));
        assert_eq!(dest.get("source").map(|v| v.as_ref()), Some("survey"));
        assert_eq!(dest.get("width").map(|v| v.as_ref()), Some("1.5"));
        assert_eq!(dest.get("source:width").map(|v| v.as_ref()), Some("survey"));
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
    use crate::lang::extract::Extract;

    fn tags<'a>(pairs: &[(&'a str, &'a str)]) -> RawTags<'a> {
        pairs.iter().map(|&(k, v)| (std::borrow::Cow::Borrowed(k), std::borrow::Cow::Borrowed(v))).collect()
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
                    meta_prefixes: &["source:", "note:"],
                    guard: None,
                    record_infix_as: Some("_infix"),
                }
            }).chain([
                InputTransform::Drop { when: Filter::TagsEmpty { tags_empty: true } },
                InputTransform::TagRule {
                    output: "highway".to_owned(),
                    source: Producer::Match {
                        rules: Vec::new(),
                        default: Some(Value::String("cycleway".to_owned())),
                        annotate: Map::new(),
                        tree: None,
                    },
                },
            ]).collect();
            TransformStep::Clone(CloneStep {
                when: Some(Filter::Not { not: Box::new(Filter::Eq {
                    extract: Extract::Value { key: "highway".to_owned(), sanitize: vec![] },
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
        assert_eq!(clone_tags.get("highway").map(|v| v.as_ref()), Some("cycleway"));
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
