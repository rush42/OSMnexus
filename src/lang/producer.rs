//! The `Producer` engine — a small tree of variants that evaluates one output's value (the one
//! mechanism behind every `outputs` entry, `TopicSpec::outputs`) plus the context (`ExtractCtx`/
//! `TagSet`) and result (`Produced`) types it evaluates over. Two branch shapes (`Match`, `Parent`)
//! and two leaf/read shapes (`Extract` — a plain key/candidate-list tag read — and `Const`) —
//! everything else is JSON-only sugar, folded into one of these by `parser`'s hand-written `Deserialize` impl
//! (`parent_or_obj`, the `{"tag": ..., "or"?: ...}` shorthand, `Rule`'s bare-`Producer` shorthand
//! for an always-true rule) so it never exists as a `Producer` value here, not even transiently
//! (see `parser`'s own doc for why that folding lives in its own module rather than inline in this
//! one). A named reference — a macro,
//! a `sanitize: "<name>"`, a *shared* classifier table (`{ "shared": "<name>" }`) — is never a
//! `Producer` concept either: all three are resolved away as a `serde_json::Value`-tree rewrite
//! (`topic::load::inline_macro_refs`/`inline_sanitize_refs`/`inline_shared_producers`) before any
//! `Producer` JSON is deserialized, so `sanitize:` fields below deserialize straight into
//! `Option<Sanitizer>` and `eval` never does a registry lookup of any kind.

use std::sync::OnceLock;

use serde_json::{Map, Value};

use crate::decision_tree::{self, DecisionTree};
use crate::lang::extract::Extract;
use crate::lang::filter::{self, Filter};
use crate::osm::types::RawTags;

/// One classifier rule — `when`/`value` are already-resolved `Filter`/`Producer` (no macro or
/// named-sanitizer reference can reach here; see their own docs). What `match_rules` evaluates.
/// `Deserialize` isn't derived directly — `parser`'s hand-written impl disambiguates the on-disk
/// shorthand shapes (same treatment `Producer`/`Filter` get; see this module's own doc for why).
#[derive(Debug, Clone)]
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
/// themselves. When `tree` is `Some` (a large enough rule table — see `Producer::Match::tree`'s own
/// doc), uses it to skip straight to the surviving candidates instead of scanning every rule; falls
/// back to a plain linear scan otherwise. Same result either way — `resolve_first`'s "keep going if
/// this candidate produced nothing" walk is exactly this function's own loop, just restricted to a
/// pre-pruned, still-in-order subset.
pub fn match_rules(
    rules: &[Rule],
    tree: Option<&DecisionTree>,
    ctx: &ExtractCtx,
    own_consts: &Map<String, Value>,
) -> Option<Produced> {
    let finish = |mut produced: Produced| {
        if produced.annotate.is_empty() {
            produced.annotate = own_consts.clone();
        }
        produced
    };
    match tree {
        Some(tree) => decision_tree::resolve_first(tree, ctx, |i| rules[i].value.eval(ctx)).map(finish),
        None => {
            for rule in rules {
                if !filter::eval(&rule.when, ctx) {
                    continue;
                }
                if let Some(produced) = rule.value.eval(ctx) {
                    return Some(finish(produced));
                }
            }
            None
        }
    }
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
/// ordinary `annotations` entries, stamped by whatever built this context (see `categorize::transform::run_transform_steps`) — `Filter::AnnotationEq`
/// reads `annotations["_side"]` the same way `Filter::Eq` reads a tag. `Copy` so a producer can
/// cheaply build a variant (e.g. swapping `obj_tags` to the parent) when re-running itself against
/// a different tagset — `annotations` stays a shared reference for that reason, never `&mut`;
/// whatever step is actively writing to it (see `InputTransform::apply`) holds its own `&mut`
/// separately and only ever hands `eval`/`Producer::eval` a reborrowed `&Map`.
#[derive(Clone, Copy)]
pub struct ExtractCtx<'a> {
    pub obj_tags: &'a RawTags<'a>,
    pub parent_tags: Option<&'a RawTags<'a>>,
    pub id: &'a str,
    pub annotations: &'a Map<String, Value>,
}

/// A shared, empty annotations map — for a context that has none to offer (a neutral
/// `eval_filter` check, or a test helper) but still needs a `&Map` to satisfy `ExtractCtx`.
pub(crate) fn empty_annotations() -> &'static Map<String, Value> {
    static EMPTY: OnceLock<Map<String, Value>> = OnceLock::new();
    EMPTY.get_or_init(Map::new)
}

/// A value producer: `Match` (a rule table) or `Extract` (a leaf tag read) — see `parser`'s
/// hand-written `Deserialize` impl (targeting this type directly) for the `parent_or_obj`/`tag`+
/// `or` JSON sugar that folds into `Match` at parse time and so never appears here. `Deserialize`
/// isn't derived directly — deliberately, so a stray `#[derive(Deserialize)]` here can't
/// reintroduce a shape `parser` doesn't know about. Every named reference (`Match` rules' macros, `Extract`'s
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
    /// `match` branch supply the value. Must be tried before `Extract` below, since `rules` is a
    /// required field and so unambiguously distinguishes it. Always reads `ctx.obj_tags` — wrap in
    /// `Parent` (or `parser::parent_or_obj`) to read the parent way's tags instead.
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
        /// A discrimination net over `rules`' own `when` conditions (`decision_tree::build` with
        /// `assume_match_is_final: false` — see its own doc for why `Producer::Match` needs that
        /// mode, not `categorize`'s), or `None` for a rule table too small to be worth it. Always
        /// starts `None` at parse time — no JSON shape ever sets it — and is filled in by a
        /// post-load compile pass (`topic::runner::compile_producer_trees`) once every rule's
        /// `when`/`value` is fully macro/sanitizer-resolved. `match_rules` uses it when present,
        /// falls back to a plain linear scan otherwise; same answers either way (see
        /// `producer_tree_matches_linear`).
        tree: Option<DecisionTree>,
    },
    /// Plain tag read — always against `ctx.obj_tags` (wrap in `Parent`, or use
    /// `parser::parent_or_obj`, for the parent's tags). `extract` carries its own `sanitize` (see
    /// `Extract`'s own doc for why) —
    /// `eval` just calls `extract.read`, which already threads the full `ExtractCtx` through.
    Extract {
        extract: Extract,
        /// Companion key/values this branch contributes when it produces the value; emitted as
        /// `<output>_<k>` (e.g. `{ "source": "tag", "confidence": "high" }`).
        annotate: Map<String, Value>,
    },
    /// A literal value, independent of any tag — `Extract`'s opposite number: the other leaf a
    /// producer tree bottoms out at. Always produces; on-disk `annotate` defaults to empty (a
    /// bare JSON literal doesn't need one, since a `Const` used as a `Rule` branch inherits the
    /// enclosing `Match`'s `annotate` when its own is empty — see `match_rules`) but can be set
    /// explicitly, same as `Extract`'s.
    Const {
        value: Value,
        annotate: Map<String, Value>,
    },
    /// Re-evaluate the inner producer against the parent way's tags instead of the object's own —
    /// `None` when there is no parent. The `Filter`-side sibling of this (`Filter::Parent`)
    /// documents the same shape in more detail.
    ///
    /// A parent-or-obj read (matching the old `TagSet::ParentOrObj`/to_bool `source: parent`)
    /// isn't a variant here — it's JSON-parse sugar (`parser::parent_or_obj`) for
    /// `Match{rules: [{when: HasParent(true), value: Parent(p)}, {when: HasParent(false), value:
    /// p}]}`. That's not the same as `{"fallback": [Parent(p), p]}`: `Match`'s rule search
    /// re-evaluates each `when` independently (a matching-but-empty rule doesn't make the next
    /// rule's `when` any truer), so the second rule is only ever eligible when there's no parent —
    /// it commits to the parent tagset whenever one exists, even if `p` then finds nothing there,
    /// where the naive fallback would also retry against the object's own tags in that case.
    Parent(Box<Producer>),
}

/// Below this many rules, a `Match`'s linear scan is already cheap enough that a compiled tree
/// isn't worth it — same threshold philosophy as `decision_tree::LOOKAHEAD_MIN_CANDIDATES`. Today
/// only the shared `road` classifier (16 rules) clears it; every other producer in the repo tops out
/// at 5.
const MATCH_TREE_MIN_RULES: usize = 6;

impl Producer {
    /// Post-load pass: for any `Match` whose `rules` are numerous enough to be worth it (see
    /// `MATCH_TREE_MIN_RULES`), compile a discrimination net over their `when` conditions (see
    /// `Match::tree`'s own doc) and stash it in `tree`. Recurses into every rule's `value` and into
    /// `Parent`'s inner producer first — a large `Match` can be nested arbitrarily deep (e.g. a rule
    /// whose own value is a further `Match`). Call once per topic load
    /// (`topic::runner::TopicRunner::load`), after every macro/sanitizer/shared-producer reference is
    /// already resolved — a `Rule`'s `when`/`value` built straight from resolved JSON needs nothing
    /// further before this can run.
    pub fn compile_trees(&mut self, max_depth: usize) {
        match self {
            Producer::Match { rules, tree, .. } => {
                for rule in rules.iter_mut() {
                    rule.value.compile_trees(max_depth);
                }
                if rules.len() > MATCH_TREE_MIN_RULES {
                    let conditions: Vec<Filter> = rules.iter().map(|r| r.when.clone()).collect();
                    *tree = Some(decision_tree::build(&conditions, max_depth, false));
                }
            }
            Producer::Parent(inner) => inner.compile_trees(max_depth),
            Producer::Extract { .. } | Producer::Const { .. } => {}
        }
    }

    /// Inverse of `compile_trees` — strip every compiled tree back out, recursively. Used by the
    /// tree/linear differential test (`producer_tree_matches_linear`) to get a guaranteed-linear
    /// reference copy from an already-compiled producer, without needing a second load pass.
    #[cfg(test)]
    pub fn clear_trees(&mut self) {
        match self {
            Producer::Match { rules, tree, .. } => {
                *tree = None;
                for rule in rules.iter_mut() {
                    rule.value.clear_trees();
                }
            }
            Producer::Parent(inner) => inner.clear_trees(),
            Producer::Extract { .. } | Producer::Const { .. } => {}
        }
    }

    pub fn eval(&self, ctx: &ExtractCtx) -> Option<Produced> {
        match self {
            Producer::Match { rules, default, annotate, tree, .. } => {
                match_rules(rules, tree.as_ref(), ctx, annotate)
                    .or_else(|| default.clone().map(|value| Produced { value, annotate: annotate.clone() }))
            }

            Producer::Extract { extract, annotate } => {
                let value = extract.read(ctx)?;
                Some(Produced { value, annotate: annotate.clone() })
            }

            Producer::Const { value, annotate } => Some(Produced { value: value.clone(), annotate: annotate.clone() }),

            Producer::Parent(inner) => match ctx.parent_tags {
                None => None,
                Some(parent_tags) => inner.eval(&ExtractCtx { obj_tags: parent_tags, ..*ctx }),
            },
        }
    }
}

#[cfg(test)]
mod classify_bool_tests {
    use super::*;
    use crate::lang::filter::Filter;
    use crate::parser::TagSet;

    fn ctx<'a>(obj: &'a RawTags<'a>, parent: Option<&'a RawTags<'a>>) -> ExtractCtx<'a> {
        ExtractCtx { obj_tags: obj, parent_tags: parent, id: "", annotations: empty_annotations() }
    }

    /// A `Match` producer with one rule and a `default`, mirroring the old `FilterMatch` shape.
    /// `from` wraps it in `Parent`/`parser::parent_or_obj` when not `TagSet::Obj`.
    fn bool_producer(filter: Filter, from: TagSet) -> Producer {
        let base = Producer::Match {
            rules: vec![Rule {
                when: filter,
                value: Producer::Const { value: Value::Bool(true), annotate: Map::new() },
            }],
            default: Some(Value::Bool(false)),
            annotate: Map::new(),
            tree: None,
        };
        match from {
            TagSet::Obj => base,
            TagSet::Parent => Producer::Parent(Box::new(base)),
            TagSet::ParentOrObj => crate::parser::parent_or_obj(base),
        }
    }

    #[test]
    fn matching_filter_produces_true() {
        let obj: RawTags = [("oneway".into(), "yes".into())].into_iter().collect();
        let producer = bool_producer(
            Filter::Eq { extract: Extract::Value { key: "oneway".to_owned(), sanitize: vec![] }, eq: "yes".to_owned() },
            TagSet::Obj,
        );
        let produced = producer.eval(&ctx(&obj, None)).unwrap();
        assert_eq!(produced.value, Value::Bool(true));
    }

    #[test]
    fn non_matching_filter_produces_false() {
        let obj: RawTags = [("oneway".into(), "no".into())].into_iter().collect();
        let producer = bool_producer(
            Filter::Eq { extract: Extract::Value { key: "oneway".to_owned(), sanitize: vec![] }, eq: "yes".to_owned() },
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
