//! The boolean expression IR shared by the category linter and the decision-tree compiler:
//! atoms (`Predicate`), literals, `Expr` trees, and the normal-form rewrites (`to_nnf`,
//! `to_dnf`) plus the `Filter` → `Expr` lowering (`filter_to_expr`). Moved here from
//! `categorize::linter` so the compiler passes (`decision_tree`) and the runtime (`topic::model`)
//! no longer depend on the linter to get at core IR (see `boundaries_plan.md`, step 2).

use std::ops::Bound;

use crate::lang::extract::Extract;
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
    HasKeyPrefix(String),
    HasParent,
    Prefix(String),
    Infix(String),
    Side(String),
    /// `Filter::TagsEmpty` — whether the object's own tags are (non-)empty. Practically never
    /// appears in a real category condition (it's `InputTransform::Drop`'s own mechanism), but
    /// `Filter` is one type shared by both, so `filter_to_expr` needs a total mapping regardless.
    TagsEmpty,
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
            Predicate::HasKeyPrefix(p) => vec![format!("prefix({})", p)],
            Predicate::HasParent => vec!["[parent]".to_string()],
            Predicate::Prefix(_) => vec!["[prefix]".to_string()],
            Predicate::Infix(_) => vec!["[infix]".to_string()],
            Predicate::Side(_) => vec!["[side]".to_string()],
            Predicate::TagsEmpty => vec!["[tags_empty]".to_string()],
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

        Filter::Eq { extract, eq, .. } => Expr::Lit(Literal::Pos(eq_predicate(extract, eq))),
        Filter::Exists { extract, exists: true, .. } => Expr::Lit(Literal::Pos(Predicate::Exists(extract.clone()))),
        Filter::Exists { extract, exists: false, .. } => Expr::Lit(Literal::Neg(Predicate::Exists(extract.clone()))),
        Filter::Contains { extract, contains, .. } => Expr::Lit(Literal::Pos(Predicate::Contains(extract.clone(), contains.clone()))),
        Filter::StartsWith { extract, starts_with, .. } => Expr::Lit(Literal::Pos(Predicate::StartsWith(extract.clone(), starts_with.clone()))),
        Filter::EndsWith { extract, ends_with, .. } => Expr::Lit(Literal::Pos(Predicate::EndsWith(extract.clone(), ends_with.clone()))),
        Filter::In { extract, r#in, .. } => match extract {
            Extract::Value { .. } | Extract::Virtual { .. } => {
                let exprs: Vec<_> = r#in.values().iter().map(|v| Expr::Lit(Literal::Pos(eq_predicate(extract, v)))).collect();
                Expr::Or(exprs)
            }
            Extract::Candidates { .. } => Expr::Lit(Literal::Pos(Predicate::FirstTagIn(extract.clone(), r#in.values().to_vec()))),
        },
        Filter::Parent { parent } => prefix_expr_tags(filter_to_expr(parent)),

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
        Filter::HasKeyPrefix { has_key_prefix } => Expr::Lit(Literal::Pos(Predicate::HasKeyPrefix(has_key_prefix.clone()))),
        Filter::HasParent { has_parent: true } => Expr::Lit(Literal::Pos(Predicate::HasParent)),
        Filter::HasParent { has_parent: false } => Expr::Lit(Literal::Neg(Predicate::HasParent)),
        Filter::TagsEmpty { tags_empty: true } => Expr::Lit(Literal::Pos(Predicate::TagsEmpty)),
        Filter::TagsEmpty { tags_empty: false } => Expr::Lit(Literal::Neg(Predicate::TagsEmpty)),
    }
}

/// `extract == v` as a predicate. An unsanitized `@side`/`@prefix`/`@infix` comparison becomes the
/// dedicated `Side`/`Prefix`/`Infix` predicate rather than a generic `Eq` on a virtual key: the
/// decision tree branches on those through its own side keys (`SIDE_KEY` etc.), so trees come out
/// exactly as they did when this was spelled `{"annotation": "_side", ..}`.
fn eq_predicate(extract: &Extract, v: &str) -> Predicate {
    use crate::lang::extract::VirtualKey;
    match extract {
        Extract::Virtual { field, parent: false, sanitize } if sanitize.is_empty() => match field {
            VirtualKey::Side => Predicate::Side(v.to_owned()),
            VirtualKey::Prefix => Predicate::Prefix(v.to_owned()),
            VirtualKey::Infix => Predicate::Infix(v.to_owned()),
            VirtualKey::Id | VirtualKey::Type => Predicate::Eq(extract.clone(), v.to_owned()),
        },
        _ => Predicate::Eq(extract.clone(), v.to_owned()),
    }
}

/// Prefix every tag-carrying predicate's key with `"parent_"`, for `Filter::Parent` — mirrors
/// what the old `ParentTag*` variants encoded directly. `Side`/`Prefix`/`Infix` become constants
/// (a parent is never a side object); `HasParent`/`HasKeyPrefix`/`TagsEmpty` pass through.
fn prefix_expr_tags(e: Expr) -> Expr {
    match e {
        Expr::True => Expr::True,
        Expr::False => Expr::False,
        // A parent is never a side object: its side is `self` and it has no prefix/infix (see
        // `ExtractCtx::parent_scope`), so these predicates are constants in parent scope — the same
        // answer `Filter::Parent`'s direct evaluation gives.
        Expr::Lit(Literal::Pos(p)) if parent_constant(&p).is_some() => bool_expr(parent_constant(&p).unwrap()),
        Expr::Lit(Literal::Neg(p)) if parent_constant(&p).is_some() => bool_expr(!parent_constant(&p).unwrap()),
        Expr::Lit(Literal::Pos(p)) => Expr::Lit(Literal::Pos(prefix_predicate(p))),
        Expr::Lit(Literal::Neg(p)) => Expr::Lit(Literal::Neg(prefix_predicate(p))),
        Expr::Not(x) => Expr::Not(Box::new(prefix_expr_tags(*x))),
        Expr::And(xs) => Expr::And(xs.into_iter().map(prefix_expr_tags).collect()),
        Expr::Or(xs) => Expr::Or(xs.into_iter().map(prefix_expr_tags).collect()),
    }
}

fn parent_constant(p: &Predicate) -> Option<bool> {
    match p {
        Predicate::Side(v) => Some(v == "self"),
        Predicate::Prefix(_) | Predicate::Infix(_) => Some(false),
        _ => None,
    }
}

fn bool_expr(b: bool) -> Expr {
    if b { Expr::True } else { Expr::False }
}

fn prefix_predicate(p: Predicate) -> Predicate {
    match p {
        Predicate::Eq(e, v) => Predicate::Eq(e.prefixed("parent_"), v),
        Predicate::Contains(e, v) => Predicate::Contains(e.prefixed("parent_"), v),
        Predicate::StartsWith(e, v) => Predicate::StartsWith(e.prefixed("parent_"), v),
        Predicate::EndsWith(e, v) => Predicate::EndsWith(e.prefixed("parent_"), v),
        Predicate::Exists(e) => Predicate::Exists(e.prefixed("parent_")),
        Predicate::FirstTagIn(e, vs) => Predicate::FirstTagIn(e.prefixed("parent_"), vs),
        Predicate::Num(e, op, bits) => Predicate::Num(e.prefixed("parent_"), op, bits),
        p @ (Predicate::HasKeyPrefix(_)
        | Predicate::HasParent
        | Predicate::Prefix(_)
        | Predicate::Infix(_)
        | Predicate::Side(_)
        | Predicate::TagsEmpty) => p,
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
