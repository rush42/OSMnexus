//! The boolean expression IR shared by the category linter and the decision-tree compiler:
//! atoms (`Predicate`), literals, `Expr` trees, and the normal-form rewrites (`to_nnf`,
//! `to_dnf`) plus the `Filter` → `Expr` lowering (`filter_to_expr`). Moved here from
//! `categorize::linter` so the compiler passes (`decision_tree`) and the runtime (`topic::model`)
//! no longer depend on the linter to get at core IR (see `boundaries_plan.md`, step 2).

use std::ops::Bound;

use crate::lang::extract::Extract;
use crate::lang::producer::Scope;
use crate::lang::filter::Filter;
/// An atom's tag-bearing variants carry the full `Extract` (key(s) + `sanitize` chain) rather than
/// a plain key string — a sanitized comparison tests a *derived* value, and both the overlap lint
/// and `decision_tree`'s branch-key eligibility need to know that (the lint still
/// groups purely by `Extract::tag_names()`, ignoring `sanitize`, same conservative approximation as
/// before; the decision tree now uses the whole `Extract` — `Extract::read_str` already applies
/// `sanitize` uniformly, so it's no longer unsound to branch on one). Two `Extract`s with the same
/// key(s) but a different `sanitize` chain are different atoms, by design.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Predicate {
    Eq(Extract, String),
    Contains(Extract, String),
    StartsWith(Extract, String),
    EndsWith(Extract, String),
    Exists(Extract),
    /// "First-present key among `Extract::Candidates`'s list has one of these values" — kept as its
    /// own atom (rather than expanded into an `Or` of per-key `Eq`s the way `Extract::Value` + `In`
    /// is) because first-present-wins semantics aren't expressible that way: a later key's value
    /// only matters when every earlier one is absent, not merely unequal.
    FirstTagIn(Extract, Vec<String>),
    /// Numeric comparison atom: `(value extract, op, threshold bits)`. The threshold is stored as
    /// the f64 bit pattern so the atom stays `Hash`/`Eq`/`Ord`; identity is exact-literal, which is
    /// all the overlap lint needs (it won't *prove* e.g. `lte 0.08 ⟹ lte 0.13`, only treats atoms as
    /// independent — sound but conservative).
    Num(Extract, NumOp, u64),
    /// `Filter::HasKeyPrefix` in the context the scopes (outermost first) switch to — what the
    /// `Extract::Scoped` chain is for a key read. `[]` is the object's own tags.
    HasKeyPrefix(Vec<Scope>, String),
    HasParent,
    /// `Filter::TagsEmpty` — whether the (scoped) layer is (non-)empty. Practically never appears
    /// unscoped in a real category condition (it's `InputTransform::Drop`'s own mechanism), but
    /// `Filter` is one type shared by both, so `filter_to_expr` needs a total mapping regardless.
    TagsEmpty(Vec<Scope>),
}

/// `parent_` / `annotations.` per scope, outermost first — `Extract::tag_names`'s spelling.
fn scope_label(scopes: &[Scope]) -> String {
    scopes.iter().map(|s| match s { Scope::Parent => "parent_", Scope::Annotations => "annotations." }).collect()
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum NumOp { Lt, Lte, Gt, Gte }

impl Predicate {
    pub fn tags_involved(&self) -> Vec<String> {
        match self {
            Predicate::Eq(e, _) => e.tag_names(),
            Predicate::Contains(e, _) => e.tag_names(),
            Predicate::StartsWith(e, _) => e.tag_names(),
            Predicate::EndsWith(e, _) => e.tag_names(),
            Predicate::Exists(e) => e.tag_names(),
            Predicate::FirstTagIn(e, _) => e.tag_names(),
            Predicate::Num(e, _, _) => e.tag_names(),
            Predicate::HasKeyPrefix(s, p) => vec![format!("{}prefix({p})", scope_label(s))],
            Predicate::HasParent => vec!["[parent]".to_string()],
            Predicate::TagsEmpty(s) => vec![format!("{}[tags_empty]", scope_label(s))],
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Literal {
    Pos(Predicate),
    Neg(Predicate),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Expr {
    True,
    False,
    Lit(Literal),
    Not(Box<Expr>),
    And(Vec<Expr>),
    Or(Vec<Expr>),
}

/// Convert a resolved `Filter` (no macro/named-sanitizer reference left — see `Filter`'s own doc)
/// to an overlap-analysis `Expr`. Used by both `decision_tree::build` (an already-resolved
/// `CategoriesFile`, built earlier in `topic::model::Topic::load` for unrelated reasons) and
/// the standalone overlap lint (`find_all_topic_overlaps`, which builds its own resolved
/// `CategoriesFile` via the same `topic::load` pipeline `Topic` uses). Every tag-bearing
/// `Predicate` now carries the full `Extract` (see `Predicate`'s own doc) — the overlap lint keeps
/// grouping by `tags_involved()` (still `sanitize`-blind, same conservative approximation as
/// before), while `decision_tree::build` uses the `Extract` itself to evaluate branches correctly
/// (`Extract::read_str` applies `sanitize`).
pub fn filter_to_expr(filter: &Filter) -> Expr {
    match filter {
        Filter::Bool(true) => Expr::True,
        Filter::Bool(false) => Expr::False,
        Filter::And { and } => Expr::And(and.iter().map(filter_to_expr).collect()),
        Filter::Or { or } => Expr::Or(or.iter().map(filter_to_expr).collect()),
        Filter::Not { not } => Expr::Not(Box::new(filter_to_expr(not))),

        Filter::Eq { extract, eq, .. } => Expr::Lit(Literal::Pos(Predicate::Eq(extract.clone(), eq.clone()))),
        Filter::Exists { extract, exists: true, .. } => Expr::Lit(Literal::Pos(Predicate::Exists(extract.clone()))),
        Filter::Exists { extract, exists: false, .. } => Expr::Lit(Literal::Neg(Predicate::Exists(extract.clone()))),
        Filter::Contains { extract, contains, .. } => Expr::Lit(Literal::Pos(Predicate::Contains(extract.clone(), contains.clone()))),
        Filter::StartsWith { extract, starts_with, .. } => Expr::Lit(Literal::Pos(Predicate::StartsWith(extract.clone(), starts_with.clone()))),
        Filter::EndsWith { extract, ends_with, .. } => Expr::Lit(Literal::Pos(Predicate::EndsWith(extract.clone(), ends_with.clone()))),
        Filter::In { extract, r#in, .. } => match extract {
            Extract::Value { .. } | Extract::Virtual { .. } | Extract::Scoped(..) => {
                let exprs: Vec<_> = r#in.values().iter().map(|v| Expr::Lit(Literal::Pos(Predicate::Eq(extract.clone(), v.clone())))).collect();
                Expr::Or(exprs)
            }
            Extract::Candidates { .. } => Expr::Lit(Literal::Pos(Predicate::FirstTagIn(extract.clone(), r#in.values().to_vec()))),
        },
        // `false` without a parent, whatever is inside. Without one every parent-scoped predicate is
        // false, so the scoped expression alone already says so — unless it is true with all its
        // predicates false (a `not`, say: `not exists` of an absent read), and only then is the
        // `HasParent` guard added. (Always adding it grows the bikelanes tree by ~12%.)
        Filter::Parent { parent } => {
            let inner = scope_expr(Scope::Parent, filter_to_expr(parent));
            if true_when_all_false(&inner) {
                Expr::And(vec![Expr::Lit(Literal::Pos(Predicate::HasParent)), inner])
            } else {
                inner
            }
        }
        Filter::Annotations { annotations } => scope_expr(Scope::Annotations, filter_to_expr(annotations)),

        Filter::NumRange { extract, min, max } => {
            let mut parts = Vec::with_capacity(2);
            match min {
                Bound::Included(v) => parts.push(Expr::Lit(Literal::Pos(Predicate::Num(extract.clone(), NumOp::Gte, v.to_bits())))),
                Bound::Excluded(v) => parts.push(Expr::Lit(Literal::Pos(Predicate::Num(extract.clone(), NumOp::Gt, v.to_bits())))),
                Bound::Unbounded => {}
            }
            match max {
                Bound::Included(v) => parts.push(Expr::Lit(Literal::Pos(Predicate::Num(extract.clone(), NumOp::Lte, v.to_bits())))),
                Bound::Excluded(v) => parts.push(Expr::Lit(Literal::Pos(Predicate::Num(extract.clone(), NumOp::Lt, v.to_bits())))),
                Bound::Unbounded => {}
            }
            match parts.len() {
                0 => Expr::True,
                1 => parts.into_iter().next().unwrap(),
                _ => Expr::And(parts),
            }
        }
        Filter::HasKeyPrefix { has_key_prefix } => Expr::Lit(Literal::Pos(Predicate::HasKeyPrefix(Vec::new(), has_key_prefix.clone()))),
        Filter::HasParent { has_parent: true } => Expr::Lit(Literal::Pos(Predicate::HasParent)),
        Filter::HasParent { has_parent: false } => Expr::Lit(Literal::Neg(Predicate::HasParent)),
        Filter::TagsEmpty { tags_empty: true } => Expr::Lit(Literal::Pos(Predicate::TagsEmpty(Vec::new()))),
        Filter::TagsEmpty { tags_empty: false } => Expr::Lit(Literal::Neg(Predicate::TagsEmpty(Vec::new()))),
    }
}

/// Push a `parent`/`annotations` context switch down onto every predicate of a flattened filter:
/// key reads get wrapped in `Extract::Scoped`, `has_key_prefix`/`tags_empty` get the scope
/// prepended to theirs, so each predicate reads the switched context by itself (and is distinct
/// from the same read in the outer context, as a decision-tree key). `HasParent` passes through:
/// it is the same in every scope (a parent's own `has_parent` is answered as `true`, see
/// `ExtractCtx::parent_scope`, and `Filter::Parent` lowers to `HasParent ∧ …` anyway).
fn scope_expr(scope: Scope, e: Expr) -> Expr {
    match e {
        Expr::True => Expr::True,
        Expr::False => Expr::False,
        Expr::Lit(Literal::Pos(p)) => Expr::Lit(Literal::Pos(scope_predicate(scope, p))),
        Expr::Lit(Literal::Neg(p)) => Expr::Lit(Literal::Neg(scope_predicate(scope, p))),
        Expr::Not(x) => Expr::Not(Box::new(scope_expr(scope, *x))),
        Expr::And(xs) => Expr::And(xs.into_iter().map(|x| scope_expr(scope, x)).collect()),
        Expr::Or(xs) => Expr::Or(xs.into_iter().map(|x| scope_expr(scope, x)).collect()),
    }
}

/// The value of `e` with every predicate false — what a `parent` scope's contents evaluate to
/// without a parent.
fn true_when_all_false(e: &Expr) -> bool {
    match e {
        Expr::True => true,
        Expr::False => false,
        Expr::Lit(Literal::Pos(_)) => false,
        Expr::Lit(Literal::Neg(_)) => true,
        Expr::Not(x) => !true_when_all_false(x),
        Expr::And(xs) => xs.iter().all(true_when_all_false),
        Expr::Or(xs) => xs.iter().any(true_when_all_false),
    }
}

fn scope_predicate(scope: Scope, p: Predicate) -> Predicate {
    let prepend = |mut scopes: Vec<Scope>| {
        scopes.insert(0, scope);
        scopes
    };
    match p {
        Predicate::Eq(e, v) => Predicate::Eq(e.scoped(scope), v),
        Predicate::Contains(e, v) => Predicate::Contains(e.scoped(scope), v),
        Predicate::StartsWith(e, v) => Predicate::StartsWith(e.scoped(scope), v),
        Predicate::EndsWith(e, v) => Predicate::EndsWith(e.scoped(scope), v),
        Predicate::Exists(e) => Predicate::Exists(e.scoped(scope)),
        Predicate::FirstTagIn(e, vs) => Predicate::FirstTagIn(e.scoped(scope), vs),
        Predicate::Num(e, op, bits) => Predicate::Num(e.scoped(scope), op, bits),
        Predicate::HasKeyPrefix(scopes, prefix) => Predicate::HasKeyPrefix(prepend(scopes), prefix),
        Predicate::TagsEmpty(scopes) => Predicate::TagsEmpty(prepend(scopes)),
        Predicate::HasParent => Predicate::HasParent,
    }
}

pub fn to_nnf(expr: Expr) -> Expr {
    match expr {
        Expr::True => Expr::True,
        Expr::False => Expr::False,
        Expr::Lit(lit) => Expr::Lit(lit),
        Expr::And(exprs) => Expr::And(exprs.into_iter().map(to_nnf).collect()),
        Expr::Or(exprs) => Expr::Or(exprs.into_iter().map(to_nnf).collect()),
        Expr::Not(inner) => match *inner {
            Expr::True => Expr::False,
            Expr::False => Expr::True,
            Expr::Lit(Literal::Pos(p)) => Expr::Lit(Literal::Neg(p)),
            Expr::Lit(Literal::Neg(p)) => Expr::Lit(Literal::Pos(p)),
            Expr::Not(inner2) => to_nnf(*inner2),
            Expr::And(exprs) => Expr::Or(exprs.into_iter().map(|e| to_nnf(Expr::Not(Box::new(e)))).collect()),
            Expr::Or(exprs) => Expr::And(exprs.into_iter().map(|e| to_nnf(Expr::Not(Box::new(e)))).collect()),
        },
    }
}

pub fn to_dnf(expr: Expr) -> Vec<Vec<Literal>> {
    match expr {
        Expr::True => vec![vec![]],
        Expr::False => vec![],
        Expr::Lit(lit) => vec![vec![lit]],
        Expr::Or(exprs) => {
            let mut result = vec![];
            for e in exprs {
                result.extend(to_dnf(e));
            }
            result
        }
        Expr::And(exprs) => {
            if exprs.is_empty() {
                return vec![vec![]];
            }
            let mut result = to_dnf(exprs[0].clone());
            for e in exprs.into_iter().skip(1) {
                let current_dnf = to_dnf(e);
                let mut new_result = vec![];
                for t1 in &result {
                    for t2 in &current_dnf {
                        let mut merged = t1.clone();
                        merged.extend(t2.clone());
                        new_result.push(merged);
                    }
                }
                result = new_result;
                if result.is_empty() {
                    break;
                }
            }
            result
        }
        Expr::Not(_) => unreachable!("Must be NNF before conversion to DNF"),
    }
}


#[cfg(test)]
mod scope_tests {
    use crate::decision_tree::runtime::eval_expr;
    use crate::lang::filter::{eval, Filter};
    use crate::lang::producer::{ExtractCtx, Layer};
    use crate::lang::tags::RawTags;
    use serde_json::{json, Map, Value};

    /// The flattened form (what the decision tree evaluates) agrees with direct evaluation, with and
    /// without a parent, for every scope combination — including negations under `parent`, which
    /// used to be true without a parent in the flattened form.
    #[test]
    fn flattened_scopes_agree_with_direct_evaluation() {
        let tags: RawTags = [("highway".into(), "cycleway".into())].into_iter().collect();
        let parent_tags: RawTags = [("highway".into(), "primary".into())].into_iter().collect();
        let mut ann = Map::new();
        ann.insert("_side".into(), Value::String("left".into()));
        let with_parent = ExtractCtx {
            obj_tags: &tags,
            parent_tags: Some(&parent_tags),
            id: "",
            annotations: &ann,
            elem: None,
            parent_elem: None,
            role: None,
            layer: Layer::Tags,
        };
        let orphan = ExtractCtx { parent_tags: None, ..with_parent };
        let cases = [
            json!({"parent": {"key": "highway", "eq": "primary"}}),
            json!({"parent": {"not": {"key": "a", "exists": true}}}),
            json!({"not": {"parent": {"key": "highway", "eq": "primary"}}}),
            json!({"parent": {"tags_empty": false}}),
            json!({"parent": {"has_key_prefix": "high"}}),
            json!({"parent": true}),
            json!({"annotations": {"key": "_side", "eq": "left"}}),
            json!({"annotations": {"not": {"key": "_side", "eq": "right"}}}),
            json!({"annotations": {"has_key_prefix": "_s"}}),
            json!({"annotations": {"tags_empty": true}}),
            json!({"annotations": {"key": "highway", "exists": true}}),
            json!({"annotations": {"parent": {"tags_empty": true}}}),
            json!({"parent": {"annotations": {"not": {"key": "_side", "exists": true}}}}),
            json!({"annotations": {"parent": {"not": {"key": "_side", "exists": true}}}}),
        ];
        for case in cases {
            let f: Filter = serde_json::from_value(case.clone()).unwrap();
            let e = super::filter_to_expr(&f);
            for (name, ctx) in [("with parent", &with_parent), ("orphan", &orphan)] {
                assert_eq!(eval(&f, ctx), eval_expr(&e, ctx), "{case} ({name}): {e:?}");
            }
        }
    }
}
