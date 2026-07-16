//! The `Producer` engine — a small tree of variants that evaluates one output's value (the one
//! mechanism behind every `outputs` entry, `TopicSpec::outputs`) plus the context (`ExtractCtx`/
//! `TagSet`) and result (`Produced`) types it evaluates over. Two branch shapes (`Match`, `Parent`)
//! and two leaf/read shapes (`Extract` — itself covering plain and direction-sensitive reads, see
//! `lang::extract`'s `Extract::Directed` — and `Const`) — everything else is
//! JSON-only sugar, folded into one of these by `parser`'s hand-written `Deserialize` impl
//! (`fallback`, `parent_or_obj`, the `{"tag": ...}`/`{"tag_or", "or"}` shorthands) so it never
//! exists as a `Producer` value here, not even transiently (see `parser`'s own doc for why that
//! folding lives in its own module rather than inline in this one). A named reference — a macro,
//! a `sanitize: "<name>"`, a *shared* classifier table (`{ "shared": "<name>" }`) — is never a
//! `Producer` concept either: all three are resolved away as a `serde_json::Value`-tree rewrite
//! (`topic::load::inline_macro_refs`/`inline_sanitize_refs`/`inline_shared_producers`) before any
//! `Producer` JSON is deserialized, so `sanitize:` fields below deserialize straight into
//! `Option<Sanitizer>` and `eval` never does a registry lookup of any kind.

use std::sync::OnceLock;

use serde::Deserialize;
use serde_json::{Map, Value};

use crate::lang::extract::Extract;
use crate::lang::filter::{self, Filter};
use crate::lang::sanitize::Sanitizer;
use crate::osm::types::RawTags;

/// One classifier rule — `when`/`value` are already-resolved `Filter`/`Producer` (no macro or
/// named-sanitizer reference can reach here; see their own docs), so a plain derive suffices, no
/// hand-written `parser` impl needed. What `match_rules` evaluates.
#[derive(Debug, Clone, Deserialize)]
pub struct Rule {
    pub when: Filter,
    pub value: Producer,
}

/// First rule whose `when` holds *and* whose value actually produces something, in order — a
/// matching rule that produces nothing doesn't stop the search, it just tries the next rule. This
/// is what lets `Producer::Match` subsume a plain ordered fallback chain (every rule `when: true`,
/// first branch that produces anything wins) as well as a conditional (one real `when`, then an
/// unconditional trailing rule for the "else"), in addition to the classic flat classify table.
///
/// A winning branch that carries its own `annotate` (e.g. an `Extract`'s own field, explicitly
/// set) is used as-is; a branch that produces an *empty* `annotate` (e.g. a bare `Producer::Const`
/// literal, which has no way to spell one) inherits `own_consts` — the enclosing `Producer::Match`'s
/// own `annotate` field — instead.
///
/// Evaluated against a full `ExtractCtx` — same predicate evaluator (`filter::eval`) and same
/// context shape category matching uses, so a rule's `when` can see side/prefix/infix/parent, not
/// just raw tags. Does not apply a `default` — callers needing one (`Producer::Match`) apply it
/// themselves.
pub fn match_rules(rules: &[Rule], ctx: &ExtractCtx, own_consts: &Map<String, Value>) -> Option<Produced> {
    for rule in rules {
        if !filter::eval(&rule.when, ctx) {
            continue;
        }
        if let Some(mut produced) = rule.value.eval(ctx) {
            if produced.annotate.is_empty() {
                produced.annotate = own_consts.clone();
            }
            return Some(produced);
        }
    }
    None
}

/// A produced value plus optional provenance. The `annotate` are arbitrary key/value pairs the
/// winning fallback branch contributes; each is emitted as `<field>_<k>` (e.g.
/// `source`/`confidence` → `<field>_source`/`<field>_confidence`).
#[derive(Debug, Clone)]
pub struct Produced {
    pub value: Value,
    pub annotate: Map<String, Value>,
}

/// Which tags (`obj_tags`, `parent_tags`), `id` — the row id for this object, defaulted to the
/// element's own id and overwritten for a side object (e.g.
/// `"way/123/cycleway/left"`) — and `annotations`, a read-only view of whatever engine bookkeeping
/// (see `output::rows::TopicRow::annotations`) has been attached to this object so far, so a
/// `Filter` (`Side`/`Prefix`/`Infix`/`TagsEmpty`/…) can branch on it just like it can on
/// `obj_tags`. There is no dedicated side-split-context field: `_side`/`_prefix`/`_infix` are
/// ordinary `annotations` entries, stamped by whatever built this context (see `categorize::transform::run_transform_steps`) — `Filter::Side`
/// reads `annotations["_side"]` the same way `Filter::Eq` reads a tag. `Copy` so a producer can
/// cheaply build a variant (e.g. swapping `obj_tags` to the parent) when re-running itself against
/// a different tagset — `annotations` stays a shared reference for that reason, never `&mut`;
/// whatever step is actively writing to it (see `InputTransform::apply`) holds its own `&mut`
/// separately and only ever hands `eval`/`Producer::eval` a reborrowed `&Map`.
#[derive(Clone, Copy)]
pub struct ExtractCtx<'a> {
    pub obj_tags: &'a RawTags,
    pub parent_tags: Option<&'a RawTags>,
    pub id: &'a str,
    pub annotations: &'a Map<String, Value>,
}

/// A shared, empty annotations map — for a context that has none to offer (a neutral
/// `eval_filter` check, or a test helper) but still needs a `&Map` to satisfy `ExtractCtx`.
pub(crate) fn empty_annotations() -> &'static Map<String, Value> {
    static EMPTY: OnceLock<Map<String, Value>> = OnceLock::new();
    EMPTY.get_or_init(Map::new)
}

/// Which tagset a bare `{name, from}` sanitizer-shorthand output entry reads (`topic::spec`) — every
/// tagset-scoping need here goes through `Producer::Parent`/`parent_or_obj` wrapping the base
/// producer (see their docs), never a field carried at runtime; `TagSet` is JSON vocabulary that
/// picks the wrapper, not a `Producer` field itself.
#[derive(Debug, Deserialize, Clone, Copy, Default)]
#[serde(rename_all = "snake_case")]
pub enum TagSet {
    #[default]
    Obj,
    /// Strict parent way: nothing if the object has no parent (matches old osm `parent`).
    Parent,
    /// Parent way, falling back to the object's own tags when there is no parent
    /// (matches the old yes_flag `source: parent`). Commits to the parent tagset when a
    /// parent exists — distinct from a `fallback:[{parent},{obj}]`, which would also fall
    /// through when the parent merely lacks the key.
    ParentOrObj,
    /// `ctx.annotations` instead of a tagset — lets the sanitizer-shorthand output sugar pull an
    /// engine-attached value (e.g. `_side`) into a real output field. Rejected for the sanitizer
    /// shorthand itself (see `topic::spec`) — only meaningful today via `DirectedFrom::Annotations`.
    Annotations,
}

/// Why a `Producer::Match` exists — a real authored rule table, or the runtime shape one of the
/// JSON sugars (`fallback`/`parent_or_obj`/`tag_or`) or a Rust-side synthesis (`topic::runner`'s
/// `default_value_producer`/`as_fallback_pair`) folds into. Purely informational — `eval`/`resolve`
/// never branch on it, only display code does (see `dag::render_producer`): a `Fallback`/`TagOr`
/// match's rules are always `when: true` by construction, so describing that condition on every
/// rule node is noise — what actually distinguishes each branch is its priority, not a predicate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MatchOrigin {
    /// Authored directly as `{"rules": [...], ...}` — a real rule table, not folded from anything.
    #[default]
    Rules,
    /// Desugared from `{"fallback": [...]}` (`parser`) or built directly by
    /// `topic::runner::as_fallback_pair` — every rule `when: true`, first branch that produces
    /// anything wins.
    Fallback,
    /// Desugared from `{"parent_or_obj": ...}` — see `Producer::parent_or_obj`.
    ParentOrObj,
    /// Desugared from `{"tag_or": ..., "or": ...}` — a single `when: true` rule plus `default`.
    TagOr,
    /// Built directly from a `defaults` JSON entry (`topic::runner::default_value_producer`) — no
    /// rules at all, just a `default`.
    Default,
}

/// A value producer: `Match` (a rule table) or `Extract` (a leaf tag read) — see `parser`'s
/// hand-written `Deserialize` impl (targeting this type directly) for the `fallback` JSON shape
/// that folds into `Match` at parse time and so never appears here. `Deserialize` isn't derived
/// directly — deliberately, so a stray `#[derive(Deserialize)]` here can't reintroduce a shape
/// `parser` doesn't know about. Every named reference (`Match` rules' macros, `Extract`'s
/// `sanitize`) is resolved away before this type is ever deserialized (see this module's own doc),
/// so `eval` never does a registry lookup.
#[derive(Debug, Clone)]
pub enum Producer {
    /// A data-defined first-match-wins rule table (same engine as the `road` classifier). Each
    /// rule's `value` is itself an arbitrary `Producer` — a literal (`Const`, e.g. a category id, a
    /// `minzoom` number, a filter-driven bool), a plain `Extract`, or a further nested `Match` —
    /// which is what lets this one variant also subsume conditionals and ordered fallback chains: a
    /// rule matches when its `when` holds, and — if its value is a producer that produces nothing —
    /// matching doesn't stop the search, the next rule is tried (see `match_rules`).
    /// `annotate` is the provenance a rule whose value produces an *empty* `annotate` of its own
    /// (chiefly `Const`, which has no way to spell one) inherits when it produces. With no `default`,
    /// returns `None` when no rule matches — letting a category const default or an enclosing
    /// fallback branch supply the value. Must be tried before `Extract` below, since `rules` is a
    /// required field and so unambiguously distinguishes it. Always reads `ctx.obj_tags` — wrap in
    /// `Parent`/`parent_or_obj` to read the parent way's tags instead.
    ///
    /// Rules see the same context a category condition does — tags, `side`/`prefix`/`infix`,
    /// parent, and macros (`as_category_context`) — so e.g. `{"prefix": "cycleway"}` or
    /// `{"macro": "..."}` work here exactly like in a category's own `condition`. The one
    /// remaining limitation: rules only see raw obj/parent tags, not fields derived earlier in the
    /// same pass.
    Match {
        rules: Vec<Rule>,
        default: Option<Value>,
        annotate: Map<String, Value>,
        /// Display-only provenance — see `MatchOrigin`'s own doc.
        origin: MatchOrigin,
    },
    /// Plain tag read — always against `ctx.obj_tags` (wrap in `Parent`/`parent_or_obj` for the
    /// parent's tags). `sanitize` is a sibling of `extract`, not part of it — see `Extract`'s own
    /// doc for why. `extract` itself may be a direction-sensitive read (`Extract::Directed`) — see
    /// its own doc for why that needs the full `ExtractCtx` `eval` already threads through here.
    Extract {
        extract: Extract,
        sanitize: Option<Sanitizer>,
        /// Companion key/values this branch contributes when it produces the value; emitted as
        /// `<output>_<k>` (e.g. `{ "source": "tag", "confidence": "high" }`).
        annotate: Map<String, Value>,
    },
    /// A literal value, independent of any tag — `Extract`'s opposite number: the other leaf a
    /// producer tree bottoms out at. Always produces; has no on-disk way to carry its own
    /// `annotate` (a bare JSON literal has nowhere to hang one), so it's always empty here — a
    /// `Const` used as a `Rule` branch inherits the enclosing `Match`'s `annotate` instead (see
    /// `match_rules`).
    Const {
        value: Value,
        annotate: Map<String, Value>,
    },
    /// Re-evaluate the inner producer against the parent way's tags instead of the object's own —
    /// `None` when there is no parent. The `Filter`-side sibling of this (`Filter::Parent`)
    /// documents the same shape in more detail.
    ///
    /// `ParentOrObj` (matching the old `TagSet::ParentOrObj`/yes_flag `source: parent`) isn't a
    /// variant here — it's JSON-parse sugar (`parser`) for `Match{rules: [{when:
    /// HasParent(true), value: Parent(p)}, {when: HasParent(false), value: p}]}`. That's not the
    /// same as `{"fallback": [Parent(p), p]}`: `Match`'s rule search re-evaluates each `when`
    /// independently (a matching-but-empty rule doesn't make the next rule's `when` any truer), so
    /// the second rule is only ever eligible when there's no parent — it commits to the parent
    /// tagset whenever one exists, even if `p` then finds nothing there, where the naive fallback
    /// would also retry against the object's own tags in that case.
    Parent(Box<Producer>),
}

impl Producer {
    pub fn eval(&self, ctx: &ExtractCtx) -> Option<Produced> {
        match self {
            Producer::Match { rules, default, annotate, .. } => {
                match_rules(rules, ctx, annotate)
                    .or_else(|| default.clone().map(|value| Produced { value, annotate: annotate.clone() }))
            }

            Producer::Extract { extract, sanitize, annotate } => {
                let value = extract.read(sanitize.as_ref(), ctx)?;
                Some(Produced { value, annotate: annotate.clone() })
            }

            Producer::Const { value, annotate } => Some(Produced { value: value.clone(), annotate: annotate.clone() }),

            Producer::Parent(inner) => match ctx.parent_tags {
                None => None,
                Some(parent_tags) => inner.eval(&ExtractCtx { obj_tags: parent_tags, ..*ctx }),
            },
        }
    }

    /// The `ParentOrObj` equivalent for `p` — see `Parent`'s doc for why this is built here rather
    /// than existing as its own variant. Used by `parser`'s `parent_or_obj` JSON sugar,
    /// `topic::spec`'s sanitizer-shorthand `from: parent_or_obj`, and Rust-side synthesis
    /// (`topic::runner`) that composes already-resolved producers — all three build/compose plain
    /// `Producer` values now, no separate as-parsed tier.
    pub fn parent_or_obj(p: Producer) -> Producer {
        Producer::Match {
            rules: vec![
                Rule {
                    when: Filter::HasParent { has_parent: true },
                    value: Producer::Parent(Box::new(p.clone())),
                },
                Rule { when: Filter::HasParent { has_parent: false }, value: p },
            ],
            default: None,
            annotate: Map::new(),
            origin: MatchOrigin::ParentOrObj,
        }
    }
}

#[cfg(test)]
mod classify_bool_tests {
    use super::*;
    use crate::lang::filter::Filter;

    fn ctx<'a>(obj: &'a RawTags, parent: Option<&'a RawTags>) -> ExtractCtx<'a> {
        ExtractCtx { obj_tags: obj, parent_tags: parent, id: "", annotations: empty_annotations() }
    }

    /// A `Match` producer with one rule and a `default`, mirroring the old `FilterMatch` shape.
    /// `from` wraps it in `Parent`/`parent_or_obj` when not `TagSet::Obj`.
    fn bool_producer(filter: Filter, from: TagSet) -> Producer {
        let base = Producer::Match {
            rules: vec![Rule {
                when: filter,
                value: Producer::Const { value: Value::Bool(true), annotate: Map::new() },
            }],
            default: Some(Value::Bool(false)),
            annotate: Map::new(),
            origin: MatchOrigin::Rules,
        };
        match from {
            TagSet::Obj => base,
            TagSet::Parent => Producer::Parent(Box::new(base)),
            TagSet::ParentOrObj => Producer::parent_or_obj(base),
            TagSet::Annotations => unreachable!("not exercised by these tests"),
        }
    }

    #[test]
    fn matching_filter_produces_true() {
        let obj: RawTags = [("oneway".to_owned(), "yes".to_owned())].into_iter().collect();
        let producer = bool_producer(
            Filter::Eq { extract: Extract::Value { key: "oneway".to_owned() }, sanitize: None, eq: "yes".to_owned() },
            TagSet::Obj,
        );
        let produced = producer.eval(&ctx(&obj, None)).unwrap();
        assert_eq!(produced.value, Value::Bool(true));
    }

    #[test]
    fn non_matching_filter_produces_false() {
        let obj: RawTags = [("oneway".to_owned(), "no".to_owned())].into_iter().collect();
        let producer = bool_producer(
            Filter::Eq { extract: Extract::Value { key: "oneway".to_owned() }, sanitize: None, eq: "yes".to_owned() },
            TagSet::Obj,
        );
        let produced = producer.eval(&ctx(&obj, None)).unwrap();
        assert_eq!(produced.value, Value::Bool(false));
    }

    #[test]
    fn missing_tagset_produces_none() {
        let obj = RawTags::default();
        let producer = bool_producer(Filter::Bool(true), TagSet::Parent);
        assert!(producer.eval(&ctx(&obj, None)).is_none());
    }
}

#[cfg(test)]
mod directed_extract_tests {
    use super::*;
    use crate::lang::extract::{DirectedFrom, DirectedKey};

    fn tags(pairs: &[(&str, &str)]) -> RawTags {
        pairs.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect()
    }

    fn side_annotations(side: &str) -> Map<String, Value> {
        let mut m = Map::new();
        m.insert("_side".to_owned(), Value::String(side.to_owned()));
        m
    }

    fn ctx<'a>(obj: &'a RawTags, parent: Option<&'a RawTags>, annotations: &'a Map<String, Value>) -> ExtractCtx<'a> {
        ExtractCtx { obj_tags: obj, parent_tags: parent, id: "", annotations }
    }

    fn directed(key: &str, from: DirectedFrom) -> Producer {
        Producer::Extract {
            extract: Extract::Directed { directed: DirectedKey { key: key.to_owned(), from } },
            sanitize: None,
            annotate: Map::new(),
        }
    }

    #[test]
    fn parent_source_prefers_existing_obj_value() {
        let obj = tags(&[("cycleway:lanes", "existing")]);
        let parent = tags(&[("cycleway:lanes:forward", "lane")]);
        let producer = directed("cycleway:lanes", DirectedFrom::Parent);
        let annotations = side_annotations("right");
        assert!(producer.eval(&ctx(&obj, Some(&parent), &annotations)).is_none());
    }

    #[test]
    fn parent_source_falls_back_to_bare_then_directed_key() {
        let obj = RawTags::default();
        let parent = tags(&[("cycleway:lanes:forward", "lane")]);
        let producer = directed("cycleway:lanes", DirectedFrom::Parent);
        let annotations = side_annotations("right");
        let produced = producer.eval(&ctx(&obj, Some(&parent), &annotations)).unwrap();
        assert_eq!(produced.value, Value::String("lane".to_owned()));

        let obj = RawTags::default();
        let parent = RawTags::default();
        assert!(producer.eval(&ctx(&obj, Some(&parent), &annotations)).is_none());
    }

    #[test]
    fn self_source_reads_from_obj_own_directed_key() {
        let obj = tags(&[("traffic_sign:forward", "DE:1022-10")]);
        let producer = directed("traffic_sign", DirectedFrom::Obj);
        let annotations = side_annotations("right");
        let produced = producer.eval(&ctx(&obj, None, &annotations)).unwrap();
        assert_eq!(produced.value, Value::String("DE:1022-10".to_owned()));
    }

    #[test]
    fn noop_for_self_side() {
        let obj = RawTags::default();
        let parent = tags(&[("cycleway:lanes:forward", "lane")]);
        let producer = directed("cycleway:lanes", DirectedFrom::Parent);
        let annotations = side_annotations("self");
        assert!(producer.eval(&ctx(&obj, Some(&parent), &annotations)).is_none());
    }

    #[test]
    fn handedness_flips_suffix() {
        let obj = RawTags::default();
        let parent = tags(&[("cycleway:lanes:backward", "lane")]);
        let producer = directed("cycleway:lanes", DirectedFrom::Parent);
        // Right-hand traffic (global default in tests): Side::Right reads `:forward`, not
        // `:backward` — so this should NOT match.
        let right = side_annotations("right");
        assert!(producer.eval(&ctx(&obj, Some(&parent), &right)).is_none());
        let left = side_annotations("left");
        let produced = producer.eval(&ctx(&obj, Some(&parent), &left)).unwrap();
        assert_eq!(produced.value, Value::String("lane".to_owned()));
    }
}
