//! Test-only research tool: how far is `build::build`'s greedy heuristic from an actual optimal
//! tree, under the exact cost model it's already trying to approximate (minimize the worst-case
//! leaf's surviving-candidate count, tie-broken on total node count)? `build_rec`/`choose_branch`
//! pick locally (single-ply, falling back to a bounded 2-ply lookahead only on already-large
//! leaves — see `build`'s own doc) rather than searching the whole tree, so there's no guarantee
//! that's actually optimal; this module answers that by exhaustively trying every eligible
//! branch at every node instead of just the locally-best one.
//!
//! Only tractable because real `Producer::Match`/category rule lists in this repo are small (≤16
//! rules, single-digit distinct branch keys — see the doc comment on `compare` below for the
//! largest ones on record). Full tree-*topology* enumeration is not remotely tractable in general
//! (branch choice compounds at every node); this stays cheap because `LEAF_MAX` collapses most
//! candidate sets to a leaf within a couple of levels, and because states are memoized by their
//! exact folded content, so revisiting the same candidate set via a different path is free.

use std::collections::HashMap;

use rustc_hash::FxHashSet;

use super::build::{
    atom_children, branch_key_candidates, eligible_atoms, eligible_values, initial_candidates, key_children,
    Candidate, LEAF_MAX,
};
use super::{BranchKey, DecisionTree};
use crate::categorize::linter::Predicate;
use crate::lang::filter::Filter;

/// `(worst_leaf_size, node_count)`, compared lexicographically — minimize the worst leaf first
/// (the same thing `best_single_branch` greedily approximates one node at a time), total node
/// count only as a tie-break.
type Cost = (usize, usize);

/// Canonical string key for memoizing a candidate set's optimal cost — `Expr` isn't `Hash`, and a
/// `Debug` dump of each candidate's folded `residual`/`own`, index-sorted, is a cheap-enough stand-in
/// at this scale (≤16 candidates per call site).
fn candidate_key(candidates: &[Candidate]) -> String {
    let mut parts: Vec<String> = candidates.iter().map(|(i, r, o)| format!("{i}:{r:?}:{o:?}")).collect();
    parts.sort();
    parts.join("|")
}

/// Exhaustive: unlike `choose_branch`, tries every eligible key/atom at this node (not just the
/// locally-best one), fully resolves each choice's whole subtree, and picks whichever minimizes
/// the resulting `Cost` — a true minimax over the entire remaining tree, not a single-ply proxy.
fn search(
    candidates: Vec<Candidate>,
    used: &mut FxHashSet<BranchKey>,
    used_atoms: &mut FxHashSet<Predicate>,
    depth: usize,
    max_depth: usize,
    memo: &mut HashMap<String, Cost>,
) -> Cost {
    if candidates.len() <= LEAF_MAX || depth >= max_depth {
        return (candidates.len(), 1);
    }
    // Keyed on remaining depth budget, not absolute depth: the same candidate content reached via
    // two different paths has the same optimal subtree either way *as long as* both have the same
    // depth budget left — but a shallower-reached (more budget) visit isn't interchangeable with a
    // deeper-reached (less budget) one, since the latter may be forced to leaf out sooner. Using
    // absolute `depth` here previously let a cost computed with more remaining budget get reused
    // for a call with less, silently inflating some subtrees' cost.
    let key = format!("{}@{}", max_depth - depth, candidate_key(&candidates));
    if let Some(cost) = memo.get(&key) {
        return *cost;
    }

    // Staying a plain leaf (no further branching) is always an option, same as `choose_branch`
    // falling back to `None` when nothing reduces the set — without this floor, `search` would
    // happily pick a branch that doesn't even shrink the worst case, paying extra nodes for zero
    // benefit over just leaving `candidates` as one leaf.
    let mut best: Cost = (candidates.len(), 1);

    for tag in branch_key_candidates(&candidates, used) {
        let values = eligible_values(&candidates, &tag);
        used.insert(tag.clone());
        let (worst, nodes) = key_children(&candidates, &tag, &values).into_iter().fold(
            (0, 1),
            |(worst, nodes), child| {
                let (l, n) = search(child, used, used_atoms, depth + 1, max_depth, memo);
                (worst.max(l), nodes + n)
            },
        );
        used.remove(&tag);
        best = best.min((worst, nodes));
    }

    for atom in eligible_atoms(&candidates, used_atoms) {
        let (on_true, on_false) = atom_children(&candidates, &atom);
        used_atoms.insert(atom.clone());
        let (l1, n1) = search(on_true, used, used_atoms, depth + 1, max_depth, memo);
        let (l2, n2) = search(on_false, used, used_atoms, depth + 1, max_depth, memo);
        used_atoms.remove(&atom);
        best = best.min((l1.max(l2), 1 + n1 + n2));
    }

    memo.insert(key, best);
    best
}

fn optimal_cost(conditions: &[Filter], max_depth: usize, assume_match_is_final: bool) -> Cost {
    let candidates = initial_candidates(conditions, assume_match_is_final);
    let mut memo = HashMap::new();
    search(candidates, &mut FxHashSet::default(), &mut FxHashSet::default(), 0, max_depth, &mut memo)
}

/// Same `Cost` metric, walked off an already-built tree (`build::build`'s actual greedy output) —
/// `DecisionTree::stats()` tracks average/total leaf size, not the worst-case single leaf this
/// module cares about, so this walks fresh rather than extending that production-facing type for
/// a test-only need.
fn tree_cost(tree: &DecisionTree) -> Cost {
    match tree {
        DecisionTree::Leaf(candidates) => (candidates.len(), 1),
        DecisionTree::Branch { children, wildcard, .. } => {
            let (mut worst, mut nodes) = tree_cost(wildcard);
            nodes += 1;
            for child in children.values() {
                let (l, n) = tree_cost(child);
                worst = worst.max(l);
                nodes += n;
            }
            (worst, nodes)
        }
        DecisionTree::AtomBranch { on_true, on_false, .. } => {
            let (l1, n1) = tree_cost(on_true);
            let (l2, n2) = tree_cost(on_false);
            (l1.max(l2), 1 + n1 + n2)
        }
    }
}

fn greedy_cost(conditions: &[Filter], max_depth: usize, assume_match_is_final: bool) -> Cost {
    tree_cost(&super::build(conditions, max_depth, assume_match_is_final))
}

/// Compare greedy vs. optimal on `conditions`, printing the result (`cargo test -- --nocapture` to
/// see it) rather than asserting — the greedy heuristic is deliberately allowed to be suboptimal
/// (that's the whole point of it being a bounded local search), so this is inspection tooling, not
/// a regression gate.
fn compare(label: &str, conditions: &[Filter], max_depth: usize) {
    let (greedy_worst, greedy_nodes) = greedy_cost(conditions, max_depth, false);
    let (optimal_worst, optimal_nodes) = optimal_cost(conditions, max_depth, false);
    println!(
        "{label}: {} rules — greedy: worst-leaf={greedy_worst} nodes={greedy_nodes} | optimal: worst-leaf={optimal_worst} nodes={optimal_nodes}",
        conditions.len(),
    );
    assert!(optimal_worst <= greedy_worst, "{label}: optimal search found a worse tree than greedy — search bug");
}

/// Pulls each `Filter` in `producers.json`'s `"road"` `match` block for a given config file's
/// `when` list, greedy vs. optimal, on the two largest real `Match` rule tables in this repo
/// (16 rules each, `configs/tilda/producers.json` and `configs/public_transport/producers.json` —
/// identical today, kept as two data points since they're separate configs free to diverge) plus
/// the largest `bikelanes` ones for a second, smaller data point.
#[test]
fn greedy_vs_optimal_on_real_configs() {
    for (label, path) in [
        ("tilda/producers.json:road", "configs/tilda/producers.json"),
        ("public_transport/producers.json:road", "configs/public_transport/producers.json"),
    ] {
        let conditions = load_road_rule_conditions(path);
        compare(label, &conditions, crate::config::DEFAULT_TREE_MAX_DEPTH);
    }

    for (label, path, field) in [
        ("bikelanes/producers.json:oneway", "configs/tilda/bikelanes/producers.json", "oneway"),
        ("bikelanes/producers.json:smoothness", "configs/tilda/bikelanes/producers.json", "smoothness"),
    ] {
        let conditions = load_match_rule_conditions(path, field);
        compare(label, &conditions, crate::config::DEFAULT_TREE_MAX_DEPTH);
    }
}

/// Loads `producers.json`'s top-level `"road"` entry (self-contained plain `Producer` JSON, no
/// macro/named-sanitizer/shared-producer references to resolve) and pulls out its `match` rules'
/// `when` conditions, in priority order — exactly the `&[Filter]` `decision_tree::build` compiles.
fn load_road_rule_conditions(path: &str) -> Vec<Filter> {
    load_match_rule_conditions(path, "road")
}

fn load_match_rule_conditions(path: &str, field: &str) -> Vec<Filter> {
    let raw = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {path}: {e}"));
    let json: serde_json::Value = serde_json::from_str(&raw).unwrap_or_else(|e| panic!("parse {path}: {e}"));
    let producer: crate::lang::producer::Producer =
        serde_json::from_value(json[field].clone()).unwrap_or_else(|e| panic!("{path}:{field}: {e}"));
    match producer {
        crate::lang::producer::Producer::Match { rules, .. } => rules.into_iter().map(|r| r.when).collect(),
        other => panic!("{path}:{field}: expected a Match producer, got {other:?}"),
    }
}
