//! Three-valued (Kleene) build-time folding — deciding a predicate under one just-branched fact,
//! leaving everything else `Unknown`, unlike `runtime::decide_concrete`'s full per-object decision.
//! This is what lets `build::build_rec` prove a candidate provably dead (or provably resolved)
//! along one branch path without needing a concrete object.

use super::{num_matches, BranchKey, HAS_PARENT_KEY, INFIX_KEY, PREFIX_KEY, SIDE_KEY};
use crate::categorize::linter::{Expr, Literal, Predicate};

#[derive(PartialEq, Eq, Clone, Copy)]
pub(crate) enum K {
    F,
    U,
    T,
}

fn not_k(k: K) -> K {
    match k {
        K::F => K::T,
        K::U => K::U,
        K::T => K::F,
    }
}

pub(crate) fn kleene(e: &Expr, decide: &impl Fn(&Predicate) -> K) -> K {
    match e {
        Expr::True => K::T,
        Expr::False => K::F,
        Expr::Lit(Literal::Pos(p)) => decide(p),
        Expr::Lit(Literal::Neg(p)) => not_k(decide(p)),
        Expr::Not(inner) => not_k(kleene(inner, decide)),
        // AND = min (any F ⇒ F, else any U ⇒ U, else T).
        Expr::And(xs) => xs.iter().fold(K::T, |acc, x| min_k(acc, kleene(x, decide))),
        // OR = max (any T ⇒ T, else any U ⇒ U, else F).
        Expr::Or(xs) => xs.iter().fold(K::F, |acc, x| max_k(acc, kleene(x, decide))),
    }
}

fn min_k(a: K, b: K) -> K {
    if a == K::F || b == K::F { K::F } else if a == K::U || b == K::U { K::U } else { K::T }
}
fn max_k(a: K, b: K) -> K {
    if a == K::T || b == K::T { K::T } else if a == K::U || b == K::U { K::U } else { K::F }
}

pub(crate) fn keep_for(expr: &Expr, decide: &impl Fn(&Predicate) -> K) -> bool {
    kleene(expr, decide) != K::F
}

/// Constant-fold `e` under `decide`: replace every literal `decide` can resolve with `True`/`False`
/// and normalize `Not`/`And`/`Or` (an `And` with a `False` child collapses to `False`, an `Or` with
/// a `True` child collapses to `True`, singleton `And`/`Or` unwrap). This is what lets several
/// branches jointly resolve an `Or` none of them could decide alone: each branch's fact gets baked
/// into the candidate's expression, so it's already gone by the time a later branch or leaf looks.
pub(crate) fn simplify(e: &Expr, decide: &impl Fn(&Predicate) -> K) -> Expr {
    match e {
        Expr::True | Expr::False => e.clone(),
        Expr::Lit(Literal::Pos(p)) => match decide(p) {
            K::T => Expr::True,
            K::F => Expr::False,
            K::U => e.clone(),
        },
        Expr::Lit(Literal::Neg(p)) => match decide(p) {
            K::T => Expr::False,
            K::F => Expr::True,
            K::U => e.clone(),
        },
        Expr::Not(inner) => match simplify(inner, decide) {
            Expr::True => Expr::False,
            Expr::False => Expr::True,
            other => Expr::Not(Box::new(other)),
        },
        Expr::And(xs) => {
            let mut kept = Vec::new();
            for x in xs {
                match simplify(x, decide) {
                    Expr::False => return Expr::False,
                    Expr::True => {}
                    other => kept.push(other),
                }
            }
            match kept.len() {
                0 => Expr::True,
                1 => kept.into_iter().next().unwrap(),
                _ => Expr::And(kept),
            }
        }
        Expr::Or(xs) => {
            let mut kept = Vec::new();
            for x in xs {
                match simplify(x, decide) {
                    Expr::True => return Expr::True,
                    Expr::False => {}
                    other => kept.push(other),
                }
            }
            match kept.len() {
                0 => Expr::False,
                1 => kept.into_iter().next().unwrap(),
                _ => Expr::Or(kept),
            }
        }
    }
}

/// Decide a predicate under "`key`'s read value == `v`, everything else unknown". Atoms reading the
/// same `Extract` as `key` are fully decidable (we know the value); all others are `Unknown`.
pub(crate) fn decide_value(p: &Predicate, key: &BranchKey, v: &str) -> K {
    let b = |cond: bool| if cond { K::T } else { K::F };
    let extract = match key {
        BranchKey::Sentinel(SIDE_KEY) => return match p { Predicate::Side(s) => b(s == v), _ => K::U },
        BranchKey::Sentinel(HAS_PARENT_KEY) => return match p { Predicate::HasParent => b(v == "true"), _ => K::U },
        BranchKey::Sentinel(PREFIX_KEY) => return match p { Predicate::Prefix(s) => b(s == v), _ => K::U },
        BranchKey::Sentinel(INFIX_KEY) => return match p { Predicate::Infix(s) => b(s == v), _ => K::U },
        BranchKey::Sentinel(other) => unreachable!("unknown branch-key sentinel {other:?}"),
        BranchKey::Tag(extract) => extract,
    };
    match p {
        Predicate::Eq(e, w) if e == extract => b(w == v),
        Predicate::FirstTagIn(e, vals) if e == extract => b(vals.iter().any(|x| x == v)),
        Predicate::Exists(e) if e == extract => K::T,
        Predicate::Contains(e, s) if e == extract => b(v.contains(s.as_str())),
        Predicate::StartsWith(e, s) if e == extract => b(v.starts_with(s.as_str())),
        Predicate::EndsWith(e, s) if e == extract => b(v.ends_with(s.as_str())),
        Predicate::Num(e, op, bits) if e == extract => match v.trim().parse::<f64>() {
            Ok(n) => b(num_matches(op, *bits, n)),
            Err(_) => K::F,
        },
        _ => K::U,
    }
}

/// Decide a predicate under "`key`'s read value is absent, or present but not among `values`
/// (`build::eligible_values`'s result for this same branch)". Only a target value that's actually
/// in `values` can be folded to `False` here — `values` is collected from *positive*
/// Eq/FirstTagIn/Prefix/Infix occurrences only (`walkers::collect_eq_values`), so a negative-only
/// literal naming some other value was never enumerated as a child, and the wildcard object could
/// still carry exactly that value; folding it to `False` regardless (as if every nameable value
/// were enumerated) is unsound — it doesn't just miss a prune, it can make the leaf's residual
/// `Expr` claim a fact that isn't true (`runtime::eval_expr` would then trust it). Existence/
/// value-shape atoms stay `Unknown` either way (the object could still carry an un-enumerated value
/// with that shape).
pub(crate) fn decide_wildcard(p: &Predicate, key: &BranchKey, values: &[String]) -> K {
    let enumerated = |v: &str| if values.iter().any(|x| x == v) { K::F } else { K::U };
    let extract = match key {
        BranchKey::Sentinel(SIDE_KEY) => return match p { Predicate::Side(_) => K::F, _ => K::U },
        BranchKey::Sentinel(HAS_PARENT_KEY) => return match p { Predicate::HasParent => K::F, _ => K::U },
        BranchKey::Sentinel(PREFIX_KEY) => return match p { Predicate::Prefix(v) => enumerated(v), _ => K::U },
        BranchKey::Sentinel(INFIX_KEY) => return match p { Predicate::Infix(v) => enumerated(v), _ => K::U },
        BranchKey::Sentinel(other) => unreachable!("unknown branch-key sentinel {other:?}"),
        BranchKey::Tag(extract) => extract,
    };
    match p {
        Predicate::Eq(e, v) if e == extract => enumerated(v),
        Predicate::FirstTagIn(e, vals) if e == extract => {
            if vals.iter().all(|v| values.iter().any(|x| x == v)) { K::F } else { K::U }
        }
        _ => K::U,
    }
}
