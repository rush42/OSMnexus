//! The load-time compiler — everything that runs once per topic (or `Producer::Match`), never
//! per-object. Builds a `DecisionTree` from an ordered `&[Filter]` list by repeatedly picking a
//! branch (`choose_branch`) that shrinks the surviving candidate set, folding each choice's fact
//! into every candidate (`kleene::simplify`) until what's left is small enough to leave as a `Leaf`.

use rustc_hash::{FxHashMap, FxHashSet};

use super::kleene::{decide_value, decide_wildcard, keep_for, simplify, K};
use super::walkers::{collect_atom_candidates, collect_branch_keys, collect_eq_values};
use super::{BranchKey, DecisionTree, HAS_PARENT_KEY, SIDE_KEY};
use crate::categorize::linter::{filter_to_expr, to_nnf, Expr, Literal, Predicate};
use crate::lang::filter::Filter;

/// Stop branching once a candidate set shrinks to this size — below this, the eval cost of a leaf
/// walk is already cheap, so a further branch's overhead isn't worth the diminishing prune.
pub(super) const LEAF_MAX: usize = 2;

/// An order-node index paired with two expressions, both simplified so far along this build path
/// (see `kleene::simplify`): `residual` is `cond_i ∧ ¬cond_1 ∧ … ∧ ¬cond_{i-1}` (first-match's own
/// effective condition — see `build`'s doc) and is what pruning/branch-choice reasons about, so a
/// node deducible-dead *relative to earlier nodes* still gets eliminated; `own` is just `cond_i`
/// alone, with the same branch facts folded in, and is what a surviving leaf candidate actually
/// needs at eval time — first-match is already handled for free by a leaf's own (index-ascending,
/// first-hit-wins) candidate order, the same way `categorize_linear`'s loop gets it from iteration
/// order alone, so `own` never needs the `¬cond_1…` conjuncts `residual` carries only for *this*
/// build's pruning. Both fold in lockstep off the same just-decided fact (see `fold_candidates`), so
/// `own` stays exactly as reduced as `residual` — just without the redundant-at-eval-time prior
/// negations.
pub(super) type Candidate = (usize, Expr, Expr);

/// Build a discrimination net over an already-ordered list of `Filter` conditions — `categorize`'s
/// compiled category/skip `order` (excludes-topo-sorted first) and a `Producer::Match`'s own `rules`
/// (already in final priority order as authored, no reordering needed) are both just "first-match
/// over an ordered `Filter` list" once you're at this level; `max_depth` caps how many tags deep a
/// branch chain may go (see `build_rec`).
///
/// `assume_match_is_final` controls whether a node's *own* condition being true is enough to prove
/// it's the answer:
/// - `true` (categorize): matching is always decisive, so node `i`'s effective condition folds in
///   every earlier node's negation (`cond_i ∧ ¬cond_1 ∧ … ∧ ¬cond_{i-1}`) — this both hands the tree
///   builder a smaller residual to branch on and, when it collapses to `Expr::False` outright,
///   proves node `i` is fully shadowed (dead) before the tree is even built.
/// - `false` (`Producer::Match`): a matching rule can still produce nothing and fall through to the
///   next one (`match_rules`'s own doc) — whether rule `j` "wins" depends on more than its `when`
///   being true, so assuming `¬cond_j` for every `j < i` would be unsound (it could wrongly prune `i`
///   for an object where `j` matched but didn't produce). Residual falls back to just `cond_i` alone
///   — weaker pruning (no cross-rule dead-code elimination), but the only sound option — and the
///   runtime walk (`super::resolve_first`) tries every surviving candidate in order, not just the
///   first match, to replicate `match_rules`'s "keep going if it produced nothing" exactly.
pub fn build(conditions: &[Filter], max_depth: usize, assume_match_is_final: bool) -> DecisionTree {
    let all = initial_candidates(conditions, assume_match_is_final);
    build_rec(all, &mut FxHashSet::default(), &mut FxHashSet::default(), 0, max_depth)
}

/// The `residual`/`own` prologue `build` folds into `build_rec` — split out so an alternate search
/// over the same candidate space (`decision_tree::optimal`, test-only) can start from exactly the
/// same starting state `build`'s greedy heuristic does, rather than risk a second, drifting copy of
/// this logic.
pub(super) fn initial_candidates(conditions: &[Filter], assume_match_is_final: bool) -> Vec<Candidate> {
    let exprs: Vec<Expr> = conditions.iter().map(|f| to_nnf(filter_to_expr(f))).collect();

    let residuals: Vec<Expr> = if assume_match_is_final {
        let neg_exprs: Vec<Expr> =
            exprs.iter().map(|e| to_nnf(Expr::Not(Box::new(e.clone())))).collect();
        let mut residuals: Vec<Expr> = Vec::with_capacity(exprs.len());
        for i in 0..exprs.len() {
            let mut parts = Vec::with_capacity(i + 1);
            parts.push(exprs[i].clone());
            parts.extend(neg_exprs[..i].iter().cloned());
            residuals.push(conjoin(parts));
        }
        residuals
    } else {
        exprs.clone()
    };

    residuals.into_iter().zip(exprs).enumerate()
        .map(|(i, (residual, own))| (i, residual, own))
        .collect()
}

/// Build an `And` of `parts` (each already NNF), flattening nested `And`s and collapsing to
/// `Expr::False` when two direct top-level conjuncts are exact-opposite literals of the same
/// predicate. Doesn't attempt full DNF-level contradiction detection (that's `to_dnf` +
/// `check_term_consistency`, used by the overlap lint): expanding to DNF here risks exponential
/// blowup once many prior `Or`-shaped conditions are negated and ANDed together, so this only
/// catches contradictions between literals that are already top-level conjuncts, not ones buried
/// across `Or` branches — the rest of a node's residual complexity gets resolved lazily, per
/// branch, by `kleene::simplify`/`kleene` during the tree walk below.
fn conjoin(parts: Vec<Expr>) -> Expr {
    let mut lits: Vec<Literal> = Vec::new();
    let mut rest: Vec<Expr> = Vec::new();
    let mut stack = parts;
    while let Some(p) = stack.pop() {
        match p {
            Expr::True => {}
            Expr::False => return Expr::False,
            Expr::And(xs) => stack.extend(xs),
            Expr::Lit(l) => lits.push(l),
            other => rest.push(other),
        }
    }
    for l in &lits {
        let opposite = match l {
            Literal::Pos(p) => Literal::Neg(p.clone()),
            Literal::Neg(p) => Literal::Pos(p.clone()),
        };
        if lits.contains(&opposite) {
            return Expr::False;
        }
    }
    lits.sort();
    lits.dedup();
    let mut children: Vec<Expr> = lits.into_iter().map(Expr::Lit).collect();
    children.extend(rest);
    match children.len() {
        0 => Expr::True,
        1 => children.into_iter().next().unwrap(),
        _ => Expr::And(children),
    }
}

/// The candidate sets a `Key(key)` branch would split `candidates` into — the wildcard child first,
/// then one per `values`, same order `Branch::children`/`build_rec` use. Shared by `build_rec`
/// (recurses into them for real) and `lookahead_branch` (needs the actual folded content to score a
/// 2-ply choice, not just a count). `best_single_branch`'s own single-ply scoring deliberately
/// doesn't use this: it only needs *how many* candidates would survive each child, which
/// `kleene::keep_for` answers without `fold_candidates`' `simplify` allocating a new `Expr` per
/// candidate — worth avoiding since it runs at every node just to pick a branch, most of which are
/// never taken.
pub(super) fn key_children(candidates: &[Candidate], key: &BranchKey, values: &[String]) -> Vec<Vec<Candidate>> {
    std::iter::once(fold_candidates(candidates, &|p| decide_wildcard(p, key, values)))
        .chain(values.iter().map(|v| fold_candidates(candidates, &|p| decide_value(p, key, v))))
        .collect()
}

/// The `(on_true, on_false)` candidate sets an `Atom(atom)` branch would split `candidates` into —
/// `key_children`'s counterpart for the other `BranchChoice` kind, same sharing rationale (used by
/// `build_rec` only; `best_single_branch` scores atoms via cheap `keep_for` counting, same as keys).
pub(super) fn atom_children(candidates: &[Candidate], atom: &Predicate) -> (Vec<Candidate>, Vec<Candidate>) {
    let b = |cond: bool| if cond { K::T } else { K::F };
    let on_true = fold_candidates(candidates, &|p| if p == atom { b(true) } else { K::U });
    let on_false = fold_candidates(candidates, &|p| if p == atom { b(false) } else { K::U });
    (on_true, on_false)
}

fn build_rec(
    candidates: Vec<Candidate>,
    used: &mut FxHashSet<BranchKey>,
    used_atoms: &mut FxHashSet<Predicate>,
    depth: usize,
    max_depth: usize,
) -> DecisionTree {
    if candidates.len() <= LEAF_MAX || depth >= max_depth {
        return DecisionTree::Leaf(into_leaf(candidates));
    }
    let Some(choice) = choose_branch(&candidates, used, used_atoms) else {
        return DecisionTree::Leaf(into_leaf(candidates));
    };

    match choice {
        BranchChoice::Key(tag) => {
            let values = eligible_values(&candidates, &tag);
            used.insert(tag.clone());

            let mut child_sets = key_children(&candidates, &tag, &values).into_iter();
            let wildcard = Box::new(build_rec(
                child_sets.next().expect("key_children always yields the wildcard child first"),
                used, used_atoms, depth + 1, max_depth,
            ));
            let mut children = FxHashMap::default();
            for (v, kept) in values.iter().zip(child_sets) {
                children.insert(v.clone(), build_rec(kept, used, used_atoms, depth + 1, max_depth));
            }

            used.remove(&tag);
            DecisionTree::Branch { tag, children, wildcard }
        }
        BranchChoice::Atom(atom) => {
            used_atoms.insert(atom.clone());

            let (on_true, on_false) = atom_children(&candidates, &atom);
            let on_true = Box::new(build_rec(on_true, used, used_atoms, depth + 1, max_depth));
            let on_false = Box::new(build_rec(on_false, used, used_atoms, depth + 1, max_depth));

            used_atoms.remove(&atom);
            DecisionTree::AtomBranch { atom, on_true, on_false }
        }
    }
}

/// Constant-fold every candidate's `residual` (and, in lockstep, its `own`) under `decide`, dropping
/// any whose `residual` collapses to `Expr::False` — `residual` alone decides survival (see
/// `Candidate`'s own doc); `own` is folded purely to stay just as reduced for whenever this candidate
/// does survive to a leaf.
fn fold_candidates(candidates: &[Candidate], decide: &impl Fn(&Predicate) -> K) -> Vec<Candidate> {
    candidates
        .iter()
        .filter_map(|(i, residual, own)| {
            let residual = simplify(residual, decide);
            (residual != Expr::False).then(|| (*i, residual, simplify(own, decide)))
        })
        .collect()
}

/// Drop each surviving candidate's `residual` (pruning is done, only `own` matters from here on —
/// see `Candidate`'s own doc).
fn into_leaf(candidates: Vec<Candidate>) -> Vec<(usize, Expr)> {
    candidates.into_iter().map(|(i, _residual, own)| (i, own)).collect()
}

enum BranchChoice {
    Key(BranchKey),
    Atom(Predicate),
}

/// Below this many candidates, a leaf's own `runtime::eval_expr` walk is already cheap enough that
/// `lookahead_branch`'s extra build-time search isn't worth it (see its own doc) — matches the
/// threshold `bin/dump_leaves`-style inspection used to call a leaf "large" in practice.
const LOOKAHEAD_MIN_CANDIDATES: usize = 6;

/// Pick the branch (tag/context key, or single atom) whose worst-case child is smallest, provided
/// it prunes at all; falls back to `lookahead_branch` for a still-large candidate set no single
/// branch improves. `None` if nothing helps.
fn choose_branch(
    candidates: &[Candidate],
    used: &FxHashSet<BranchKey>,
    used_atoms: &FxHashSet<Predicate>,
) -> Option<BranchChoice> {
    if let Some((choice, _)) = best_single_branch(candidates, used, used_atoms) {
        return Some(choice);
    }
    if candidates.len() > LOOKAHEAD_MIN_CANDIDATES {
        return lookahead_branch(candidates, used, used_atoms);
    }
    None
}

/// Bounded 2-ply fallback, only tried on an already-large candidate set (see
/// `LOOKAHEAD_MIN_CANDIDATES`) that no single branch shrinks at all: try each remaining key `key1`
/// anyway, and see whether some *second* branch shrinks the worst resulting child — several
/// branches can jointly resolve an `Or` spanning multiple keys that no single one of them could
/// decide alone (see `kleene::simplify`'s own doc; `is_protected_bikelane_separation`-shaped
/// conditions, `Or`ing across `separation*` and `traffic_mode*`, are exactly this case). Only
/// considers `Key` branches for `key1` (not atoms) — atoms are already single literal splits with
/// little left for a second branch to jointly resolve. Tried only above the size threshold: below
/// it, this ran on *every* no-single-branch-helps leaf tree-wide and bloated the tree ~11x for a
/// handful of leaves actually worth it (measured on `bikelanes/way`: 83→71 leaves >6 candidates,
/// 247→2774 nodes) — gating it to already-large leaves keeps the search where the payoff is.
fn lookahead_branch(
    candidates: &[Candidate],
    used: &FxHashSet<BranchKey>,
    used_atoms: &FxHashSet<Predicate>,
) -> Option<BranchChoice> {
    let tags = branch_key_candidates(candidates, used);

    let mut best: Option<BranchChoice> = None;
    let mut best_worst = candidates.len();
    for key1 in tags {
        let values = eligible_values(candidates, &key1);
        let mut used1 = used.clone();
        used1.insert(key1.clone());

        let child_worst = |child: &[Candidate]| match best_single_branch(child, &used1, used_atoms) {
            Some((_, w)) => w,
            None => child.len(),
        };
        let worst = key_children(candidates, &key1, &values).iter().map(|child| child_worst(child)).max().unwrap_or(0);

        if worst < best_worst {
            best_worst = worst;
            best = Some(BranchChoice::Key(key1));
        }
    }
    best
}

/// The set of eligible branch keys across `candidates` — tags (plain, sanitized, or parent-scoped)
/// appearing in a positive Eq/FirstTagIn atom, plus the `side`/`has_parent`/`prefix`/`infix`
/// sentinels wherever those predicates appear at all. Shared by `best_single_branch` and
/// `lookahead_branch`.
pub(super) fn branch_key_candidates(candidates: &[Candidate], used: &FxHashSet<BranchKey>) -> FxHashSet<BranchKey> {
    let mut tags: FxHashSet<BranchKey> = FxHashSet::default();
    for (_, e, _) in candidates {
        collect_branch_keys(e, &mut |k| {
            if !used.contains(&k) {
                tags.insert(k);
            }
        });
    }
    tags
}

/// Candidate atoms: total (non-Eq) predicates — `Contains`, `StartsWith`, `EndsWith`, `Num`,
/// `Exists`, `HasKeyPrefix` — either polarity, since a missing tag decides them outright (false)
/// rather than leaving them unknown. Shared by `best_single_branch` and the test-only optimal
/// search (`decision_tree::optimal`), same rationale as `branch_key_candidates`.
pub(super) fn eligible_atoms(candidates: &[Candidate], used_atoms: &FxHashSet<Predicate>) -> FxHashSet<Predicate> {
    let mut atoms: FxHashSet<Predicate> = FxHashSet::default();
    for (_, e, _) in candidates {
        collect_atom_candidates(e, &mut |p| {
            if !used_atoms.contains(p) {
                atoms.insert(p.clone());
            }
        });
    }
    atoms
}

/// The best single branch (tag/context key, or atom) whose worst-case child is smallest, alongside
/// that worst-case size — `None` if nothing reduces the candidate set at all in one step.
fn best_single_branch(
    candidates: &[Candidate],
    used: &FxHashSet<BranchKey>,
    used_atoms: &FxHashSet<Predicate>,
) -> Option<(BranchChoice, usize)> {
    let mut best: Option<BranchChoice> = None;
    let mut best_worst = candidates.len(); // require strictly smaller than the full set

    let tags = branch_key_candidates(candidates, used);
    for key in tags {
        let values = eligible_values(candidates, &key);
        let mut worst =
            candidates.iter().filter(|(_, e, _)| keep_for(e, &|p| decide_wildcard(p, &key, &values))).count();
        for v in &values {
            let c = candidates.iter().filter(|(_, e, _)| keep_for(e, &|p| decide_value(p, &key, v))).count();
            worst = worst.max(c);
        }
        if worst < best_worst {
            best_worst = worst;
            best = Some(BranchChoice::Key(key));
        }
    }

    for atom in eligible_atoms(candidates, used_atoms) {
        let b = |cond: bool| if cond { K::T } else { K::F };
        let t = candidates.iter().filter(|(_, e, _)| keep_for(e, &|p| if p == &atom { b(true) } else { K::U })).count();
        let f = candidates.iter().filter(|(_, e, _)| keep_for(e, &|p| if p == &atom { b(false) } else { K::U })).count();
        let worst = t.max(f);
        if worst < best_worst {
            best_worst = worst;
            best = Some(BranchChoice::Atom(atom));
        }
    }

    best.map(|b| (b, best_worst))
}

/// All exact-eq values of `key` across the candidate conditions. For the `side`/`has_parent`
/// sentinels the domain is fixed and known up front (no need to scan conditions for it).
pub(super) fn eligible_values(candidates: &[Candidate], key: &BranchKey) -> Vec<String> {
    match key {
        BranchKey::Sentinel(SIDE_KEY) => return vec!["left".to_string(), "right".to_string(), "self".to_string()],
        BranchKey::Sentinel(HAS_PARENT_KEY) => return vec!["false".to_string(), "true".to_string()],
        _ => {}
    }
    let mut vals: FxHashSet<String> = FxHashSet::default();
    for (_, e, _) in candidates {
        collect_eq_values(e, key, &mut |v| {
            vals.insert(v.to_string());
        });
    }
    let mut out: Vec<String> = vals.into_iter().collect();
    out.sort();
    out
}
