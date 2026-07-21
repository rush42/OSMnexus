//! Small `Expr`-tree scans that `build` uses to find branch-key/atom candidates. Pure syntactic
//! walks — no build-time folding (that's `kleene`) and no per-object evaluation (that's `runtime`).

use super::{BranchKey, HAS_PARENT_KEY, INFIX_KEY, PREFIX_KEY, SIDE_KEY};
use crate::categorize::linter::{Expr, Literal, Predicate};
use crate::lang::extract::Extract;

/// Invoke `f` on the `Extract` of every positive `Eq`/`FirstTagIn` atom, and on the relevant
/// sentinel key for every `Side`/`HasParent`/`Prefix`/`Infix` atom (either polarity — their fixed,
/// small domain makes them decidable regardless of sign, unlike an arbitrary-string tag `Eq`/`Neq`).
pub(crate) fn collect_branch_keys(e: &Expr, f: &mut impl FnMut(BranchKey)) {
    match e {
        Expr::Lit(Literal::Pos(Predicate::Eq(extract, _) | Predicate::FirstTagIn(extract, _))) => {
            f(BranchKey::Tag(extract.clone()))
        }
        Expr::Lit(Literal::Pos(Predicate::Side(_)) | Literal::Neg(Predicate::Side(_))) => {
            f(BranchKey::Sentinel(SIDE_KEY))
        }
        Expr::Lit(Literal::Pos(Predicate::HasParent) | Literal::Neg(Predicate::HasParent)) => {
            f(BranchKey::Sentinel(HAS_PARENT_KEY))
        }
        Expr::Lit(Literal::Pos(Predicate::Prefix(_)) | Literal::Neg(Predicate::Prefix(_))) => {
            f(BranchKey::Sentinel(PREFIX_KEY))
        }
        Expr::Lit(Literal::Pos(Predicate::Infix(_)) | Literal::Neg(Predicate::Infix(_))) => {
            f(BranchKey::Sentinel(INFIX_KEY))
        }
        Expr::Lit(_) | Expr::True | Expr::False => {}
        Expr::Not(x) => collect_branch_keys(x, f),
        Expr::And(xs) | Expr::Or(xs) => xs.iter().for_each(|x| collect_branch_keys(x, f)),
    }
}

/// Invoke `f` on every `Contains`/`StartsWith`/`EndsWith`/`Num`/`Exists`/`HasKeyPrefix` atom (either
/// polarity) — the atom kinds `AtomBranch` can use. Parent-scoped atoms are included:
/// `runtime::eval_atom` reads through `read_extract`/`read_extract_num`, which redirect a
/// parent-scoped `Extract` to `ctx.parent_tags`, so branching on one is exactly as sound as a
/// plain-tag atom.
pub(crate) fn collect_atom_candidates(e: &Expr, f: &mut impl FnMut(&Predicate)) {
    match e {
        Expr::Lit(Literal::Pos(p) | Literal::Neg(p)) => {
            match p {
                Predicate::Contains(..)
                | Predicate::StartsWith(..)
                | Predicate::EndsWith(..)
                | Predicate::Exists(..)
                | Predicate::Num(..)
                | Predicate::HasKeyPrefix(..) => f(p),
                _ => {}
            }
        }
        Expr::True | Expr::False => {}
        Expr::Not(x) => collect_atom_candidates(x, f),
        Expr::And(xs) | Expr::Or(xs) => xs.iter().for_each(|x| collect_atom_candidates(x, f)),
    }
}

/// Invoke `f` on the value(s) of every positive `Eq`/`FirstTagIn` atom on `key`'s `Extract` (or, for
/// `PREFIX_KEY`/`INFIX_KEY`, every positive `Prefix`/`Infix` atom's literal value).
pub(crate) fn collect_eq_values(e: &Expr, key: &BranchKey, f: &mut impl FnMut(&str)) {
    let matches_extract = |e: &Extract| matches!(key, BranchKey::Tag(k) if k == e);
    match e {
        Expr::Lit(Literal::Pos(Predicate::Eq(extract, v))) if matches_extract(extract) => f(v),
        Expr::Lit(Literal::Pos(Predicate::FirstTagIn(extract, vs))) if matches_extract(extract) => {
            vs.iter().for_each(|v| f(v))
        }
        Expr::Lit(Literal::Pos(Predicate::Prefix(v))) if matches!(key, BranchKey::Sentinel(PREFIX_KEY)) => f(v),
        Expr::Lit(Literal::Pos(Predicate::Infix(v))) if matches!(key, BranchKey::Sentinel(INFIX_KEY)) => f(v),
        Expr::Lit(_) | Expr::True | Expr::False => {}
        Expr::Not(x) => collect_eq_values(x, key, f),
        Expr::And(xs) | Expr::Or(xs) => xs.iter().for_each(|x| collect_eq_values(x, key, f)),
    }
}
