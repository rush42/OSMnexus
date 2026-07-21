//! The per-object evaluator — everything that runs once per element, as opposed to `build`'s
//! load-time compiler. `eval_expr`/`resolve_first` are the two entry points callers outside this
//! module use (`categorize`, `Producer::Match`); the rest (`eval_atom`, `branch_key`,
//! `decide_concrete`, parent-scope redirection) only exist to serve those two.

use std::borrow::Cow;

use serde_json::Value;

use super::{num_matches, BranchKey, DecisionTree, HAS_PARENT_KEY, INFIX_KEY, PREFIX_KEY, SIDE_KEY};
use crate::categorize::linter::{Expr, Literal, Predicate};
use crate::lang::extract::Extract;
use crate::lang::filter::read_num;
use crate::lang::producer::ExtractCtx;

/// Evaluate a single atom directly against the object's context — `AtomBranch`'s own entry point
/// into `decide_concrete` (every atom kind `AtomBranch` is ever built from —
/// `Contains`/`StartsWith`/`EndsWith`/`Num`/`Exists`/`HasKeyPrefix` — is one `decide_concrete` already
/// handles identically; a separate match here would just duplicate those arms verbatim).
pub(crate) fn eval_atom(atom: &Predicate, ctx: &ExtractCtx) -> bool {
    decide_concrete(atom, ctx)
}

/// `Extract` naming a synthetic `parent_<key>` tag (the name `prefix_expr_tags` stamps for a
/// `Filter::Parent` condition) — `read_extract`/`read_extract_num` redirect these to
/// `ctx.parent_tags` instead of `ctx.obj_tags`. Matches directly on `Extract` rather than going
/// through `tag_names()` — this runs on every predicate decision in `eval_expr`'s per-object hot
/// path, so it can't afford `tag_names()`'s `Vec<String>` allocation for what's almost always a
/// `false` answer.
fn parent_scoped(extract: &Extract) -> bool {
    match extract {
        Extract::Value { key, .. } => key.starts_with("parent_"),
        Extract::Candidates { keys, .. } => keys.iter().any(|k| k.starts_with("parent_")),
    }
}

/// Read `extract` against `ctx`, transparently switching to `ctx.parent_tags` for a parent-scoped
/// `Extract` (see `parent_scoped`). The leaf-time evaluator (`eval_expr`) works from the flattened
/// `Expr` — no `Filter::Parent` wrapper survives into it, just every key renamed `parent_<key>` by
/// `categorize::linter::prefix_expr_tags` — so this is what replays `Filter::Parent`'s own
/// `None => false, Some(parent_tags) => eval(parent, parent_tags)` semantics for a single predicate.
fn read_extract<'a>(extract: &Extract, ctx: &ExtractCtx<'a>) -> Option<Cow<'a, str>> {
    if parent_scoped(extract) {
        let parent_tags = ctx.parent_tags?;
        let parent_ctx = ExtractCtx { obj_tags: parent_tags, parent_tags: ctx.parent_tags, id: ctx.id, annotations: ctx.annotations };
        extract.strip_prefix("parent_").read_str(&parent_ctx)
    } else {
        extract.read_str(ctx)
    }
}

/// `read_num`'s counterpart to `read_extract` — same parent-scope switch, for `Predicate::Num`.
fn read_extract_num(extract: &Extract, ctx: &ExtractCtx) -> Option<f64> {
    if parent_scoped(extract) {
        let parent_tags = ctx.parent_tags?;
        let parent_ctx = ExtractCtx { obj_tags: parent_tags, parent_tags: ctx.parent_tags, id: ctx.id, annotations: ctx.annotations };
        read_num(&extract.strip_prefix("parent_"), &parent_ctx)
    } else {
        read_num(extract, ctx)
    }
}

/// Fully decide one predicate against a concrete object — unlike `kleene::decide_value`/
/// `decide_wildcard` (partial, build-time, three-valued under one known fact), every predicate is
/// decidable here since `ctx` is a real object. Mirrors `lang::filter::eval`'s per-`Filter`-variant
/// semantics exactly (see each arm), since this is what stands in for a full `Filter` re-eval at
/// leaf time.
fn decide_concrete(p: &Predicate, ctx: &ExtractCtx) -> bool {
    match p {
        Predicate::Eq(e, v) => read_extract(e, ctx).is_some_and(|s| s.as_ref() == v.as_str()),
        Predicate::Contains(e, s) => read_extract(e, ctx).is_some_and(|v| v.contains(s.as_str())),
        Predicate::StartsWith(e, s) => read_extract(e, ctx).is_some_and(|v| v.starts_with(s.as_str())),
        Predicate::EndsWith(e, s) => read_extract(e, ctx).is_some_and(|v| v.ends_with(s.as_str())),
        Predicate::Exists(e) => read_extract(e, ctx).is_some(),
        Predicate::FirstTagIn(e, vals) => read_extract(e, ctx).is_some_and(|v| vals.iter().any(|x| x == v.as_ref())),
        Predicate::Num(e, op, bits) => read_extract_num(e, ctx).is_some_and(|n| num_matches(op, *bits, n)),
        Predicate::HasKeyPrefix(prefix) => ctx.obj_tags.keys().any(|k| k.starts_with(prefix.as_str())),
        Predicate::HasParent => ctx.parent_tags.is_some(),
        // Same lookup `Filter::AnnotationEq` uses — no "self"/"absent" default (unlike `branch_key`'s
        // own `SIDE_KEY` handling, a tree-descent safety net, not `eval`'s real semantics).
        Predicate::Side(v) => ctx.annotations.get("_side").and_then(Value::as_str) == Some(v.as_str()),
        Predicate::Prefix(v) => ctx.annotations.get("_prefix").and_then(Value::as_str) == Some(v.as_str()),
        Predicate::Infix(v) => ctx.annotations.get("_infix").and_then(Value::as_str) == Some(v.as_str()),
        Predicate::TagsEmpty => ctx.obj_tags.is_empty(),
    }
}

/// Fully evaluate a leaf's residual `Expr` against a concrete object — the leaf-time counterpart to
/// `kleene::kleene`/`simplify` (which only ever partially decide, under one just-branched fact).
/// Every `Predicate` is decidable here, so this collapses straight to a `bool`, no `Unknown` case.
fn eval_expr(e: &Expr, ctx: &ExtractCtx) -> bool {
    match e {
        Expr::True => true,
        Expr::False => false,
        Expr::Lit(Literal::Pos(p)) => decide_concrete(p, ctx),
        Expr::Lit(Literal::Neg(p)) => !decide_concrete(p, ctx),
        Expr::Not(x) => !eval_expr(x, ctx),
        Expr::And(xs) => xs.iter().all(|x| eval_expr(x, ctx)),
        Expr::Or(xs) => xs.iter().any(|x| eval_expr(x, ctx)),
    }
}

/// Walk `tree`'s surviving candidates in order (ascending = priority), calling `on_match(i)` for
/// each whose condition is actually true; the first `Some(t)` `on_match` returns wins. Shared by
/// both callers of a compiled tree:
/// - `categorize` (`assume_match_is_final: true` trees): `on_match` always returns `Some(_)` for a
///   true condition — the first true candidate is unconditionally the answer, matching this module's
///   original single-purpose walk.
/// - `Producer::Match` (`assume_match_is_final: false` trees): `on_match` runs the rule's `value`
///   producer and returns `None` if it produces nothing, so the walk keeps trying later candidates —
///   replicating `match_rules`'s "matched but produced nothing, keep going" exactly, just restricted
///   to the (already-pruned, still-in-order) candidates the tree kept.
pub(crate) fn resolve_first<T>(
    tree: &DecisionTree,
    ctx: &ExtractCtx,
    mut on_match: impl FnMut(usize) -> Option<T>,
) -> Option<T> {
    for (i, expr) in tree.candidates(ctx) {
        if eval_expr(expr, ctx) {
            if let Some(t) = on_match(*i) {
                return Some(t);
            }
        }
    }
    None
}

/// Resolve a branch key against the object's context. `None` means "no matching enumerated child"
/// → fall through to the wildcard. `BranchKey::Tag` reads through `read_extract` (not
/// `Extract::read_str` directly) so a parent-scoped branch key redirects to `ctx.parent_tags`, same
/// as `eval_atom`/`decide_concrete`.
pub(crate) fn branch_key<'a>(ctx: &ExtractCtx<'a>, key: &BranchKey) -> Option<Cow<'a, str>> {
    match key {
        // `_side` is always stamped by `topic::pipeline::build_topic_rows`/each `Clone` (self
        // included) — defaulting to "self" here is a safety net, not the normal path.
        BranchKey::Sentinel(SIDE_KEY) => {
            Some(Cow::Borrowed(ctx.annotations.get("_side").and_then(Value::as_str).unwrap_or("self")))
        }
        BranchKey::Sentinel(HAS_PARENT_KEY) => {
            Some(Cow::Borrowed(if ctx.parent_tags.is_some() { "true" } else { "false" }))
        }
        BranchKey::Sentinel(PREFIX_KEY) => ctx.annotations.get("_prefix").and_then(Value::as_str).map(Cow::Borrowed),
        BranchKey::Sentinel(INFIX_KEY) => ctx.annotations.get("_infix").and_then(Value::as_str).map(Cow::Borrowed),
        BranchKey::Sentinel(other) => unreachable!("unknown branch-key sentinel {other:?}"),
        BranchKey::Tag(extract) => read_extract(extract, ctx),
    }
}
