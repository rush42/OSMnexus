//! Discrimination net over an ordered list of `Filter` conditions, to prune a first-match walk —
//! both the runtime tree shape/walk (`DecisionTree`, `candidates()`/`resolve_first()`, in
//! `runtime`) and the load-time compiler (`build`, in `build`; nothing in that half runs
//! per-object). Two callers share this: `categorize` (over a `CategoriesFile`'s excludes-compiled
//! `order`) and `Producer::Match` (over its own `rules`, already in final priority order as
//! authored) — see `build::build`'s own doc for the one real difference between them
//! (`assume_match_is_final`).
//!
//! The tree branches on discriminating equality-tags (chiefly `highway`) and on a handful of
//! context fields that are *always* fully known and small-domain — `side`, `has_parent`, `prefix`,
//! `infix` — descending by the object's tag values / context to a leaf holding a small,
//! order-preserving subset of the priority list, then running the existing first-match `eval` over
//! just that subset.
//!
//! **Soundness.** The tree only prunes an order-node when it is *provably false* for the branch
//! value; the leaf's authoritative `eval` decides the rest. Pruning uses a three-valued (Kleene,
//! `kleene`) evaluation of the node's NNF condition under a partial assignment: for a branch on tag
//! `T=v`, every atom mentioning `T` is decided concretely (we know the value) and all other atoms
//! are `Unknown`. If the whole expression evaluates to `False`, the node cannot match → prune it.
//! Worst case the tree prunes nothing and matches the full walk; it never changes results.
//!
//! A branch key is a `BranchKey`: either a sentinel (`side`/`has_parent`/`prefix`/`infix`, reading
//! `ExtractCtx` fields rather than a tag) or a `Tag(Extract)` wrapping the same `Extract` its
//! candidate `Predicate`s carry — `Extract::read_str` already applies `sanitize` uniformly (see its
//! own doc), so branching on a sanitized comparison is exactly as sound as an unsanitized one; no
//! separate ban is needed.
//!
//! `side`/`has_parent`/`prefix`/`infix` branch on `ExtractCtx` fields rather than `ctx.obj_tags`,
//! using sentinel keys (`SIDE_KEY` etc. — a leading NUL byte no real OSM tag can contain) so they
//! reuse the same `Branch` shape and `used`/`choose_branch` machinery as tag branching.
//!
//! `AtomBranch` handles the rest: `Contains`/`StartsWith`/`EndsWith`/`Num`/`Exists`/`HasKeyPrefix`
//! atoms are, unlike `Eq`, *total* — they evaluate to a concrete `bool` for every object, tag
//! present or not (a missing tag just makes `Contains` etc. false, mirroring `eval`'s semantics) —
//! so a single literal atom (e.g. `traffic_sign contains "237"`) can be used as its own two-way
//! branch with no wildcard/unknown case, decided by evaluating that one atom against the object.
//!
//! Split across submodules by concern: this file is just the shared types (`BranchKey`,
//! `DecisionTree`, `TreeStats`) plus the tree-shape walk (`candidates`); `runtime` holds the rest of
//! the per-object evaluator (`eval_expr`/`resolve_first`/`eval_atom`/parent-scope handling);
//! `kleene` holds the three-valued build-time folding (`kleene`/`simplify`/`decide_value`/
//! `decide_wildcard`); `walkers` holds the small `Expr`-tree scans that find branch-key/atom
//! candidates; `build` holds the load-time compiler (`build`/`choose_branch`/`lookahead_branch`).

mod build;
mod kleene;
mod runtime;
mod walkers;
#[cfg(test)]
mod tests;

use rustc_hash::FxHashMap;

use crate::categorize::linter::{Expr, NumOp, Predicate};
use crate::lang::extract::Extract;
use crate::lang::producer::ExtractCtx;

pub use build::build;
pub(crate) use runtime::resolve_first;

/// Sentinel branch key for `Predicate::Side`. Domain is exactly `{"self","left","right"}` — always
/// fully known for a given object, so this branch never needs a wildcard fallback in practice.
pub(crate) const SIDE_KEY: &str = "\0side";
/// Sentinel branch key for `Predicate::HasParent`. Domain is exactly `{"true","false"}`.
pub(crate) const HAS_PARENT_KEY: &str = "\0has_parent";
/// Sentinel branch key for `Predicate::Prefix`. Like a tag: enumerated literal values + wildcard
/// for "absent or some other prefix".
pub(crate) const PREFIX_KEY: &str = "\0prefix";
/// Sentinel branch key for `Predicate::Infix`. Same shape as `PREFIX_KEY`.
pub(crate) const INFIX_KEY: &str = "\0infix";

/// What a `DecisionTree::Branch` reads to decide which child to descend into: one of the four
/// fixed-domain `ExtractCtx` sentinels, or a real tag read (`Extract::read_str`, `sanitize` and
/// all — see this module's own doc).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum BranchKey {
    Sentinel(&'static str),
    Tag(Extract),
}

impl std::fmt::Display for BranchKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BranchKey::Sentinel(s) => write!(f, "{s}"),
            BranchKey::Tag(Extract::Value { key, sanitize }) => {
                write!(f, "{key}")?;
                if !sanitize.is_empty() { write!(f, " (sanitized)")?; }
                Ok(())
            }
            BranchKey::Tag(Extract::Candidates { keys, sanitize }) => {
                write!(f, "{}", keys.join("/"))?;
                if !sanitize.is_empty() { write!(f, " (sanitized)")?; }
                Ok(())
            }
        }
    }
}

#[derive(Debug, Clone)]
pub enum DecisionTree {
    /// Order-node index, paired with its condition as simplified along the path to this leaf (see
    /// `build::Candidate`'s own doc), in ascending (= priority) order. Each pair's `Expr` already has
    /// every fact the branches above proved baked in — `runtime::eval_expr` (not the original
    /// `Filter`) is what a leaf walk should evaluate, so it never re-reads/re-compares a tag the
    /// tree already decided.
    Leaf(Vec<(usize, Expr)>),
    Branch {
        tag: BranchKey,
        /// Child per enumerated exact-eq value of `tag`.
        children: FxHashMap<String, DecisionTree>,
        /// Object's `tag` absent or not among the enumerated values.
        wildcard: Box<DecisionTree>,
    },
    /// Two-way split on a single literal atom (e.g. `Contains("traffic_sign", "237")`), decided by
    /// evaluating that exact atom against the object. No wildcard needed — these atom kinds are
    /// total, so `on_true`/`on_false` exhaustively cover every object.
    AtomBranch {
        atom: Predicate,
        on_true: Box<DecisionTree>,
        on_false: Box<DecisionTree>,
    },
}

impl Default for DecisionTree {
    fn default() -> Self {
        DecisionTree::Leaf(Vec::new())
    }
}

/// Aggregate shape stats for a built tree, for gauging how much a `--tree-max-depth` change (or a
/// rule-set edit) is actually pruning vs. bloating the tree.
#[derive(Debug, Clone, Copy, Default)]
pub struct TreeStats {
    pub leaf_count: usize,
    pub branch_count: usize,
    /// Sum of `Leaf` candidate-slice lengths, for `total_leaf_candidates / leaf_count` = avg leaf size.
    pub total_leaf_candidates: usize,
    pub max_depth: usize,
    /// Sum of leaf depths, for `total_leaf_depth / leaf_count` = avg leaf depth.
    pub total_leaf_depth: usize,
}

impl TreeStats {
    pub fn avg_leaf_depth(&self) -> f64 {
        if self.leaf_count == 0 { 0.0 } else { self.total_leaf_depth as f64 / self.leaf_count as f64 }
    }

    pub fn avg_leaf_size(&self) -> f64 {
        if self.leaf_count == 0 { 0.0 } else { self.total_leaf_candidates as f64 / self.leaf_count as f64 }
    }
}

impl DecisionTree {
    /// Walk the tree and tally shape stats (leaf/branch counts, depth, leaf size).
    pub fn stats(&self) -> TreeStats {
        let mut s = TreeStats::default();
        self.stats_rec(0, &mut s);
        s
    }

    fn stats_rec(&self, depth: usize, s: &mut TreeStats) {
        s.max_depth = s.max_depth.max(depth);
        match self {
            DecisionTree::Leaf(idxs) => {
                s.leaf_count += 1;
                s.total_leaf_candidates += idxs.len();
                s.total_leaf_depth += depth;
            }
            DecisionTree::Branch { children, wildcard, .. } => {
                s.branch_count += 1;
                for child in children.values() {
                    child.stats_rec(depth + 1, s);
                }
                wildcard.stats_rec(depth + 1, s);
            }
            DecisionTree::AtomBranch { on_true, on_false, .. } => {
                s.branch_count += 1;
                on_true.stats_rec(depth + 1, s);
                on_false.stats_rec(depth + 1, s);
            }
        }
    }

    /// Descend to the leaf's candidate (order-node index, residual condition) slice for this object.
    pub fn candidates<'a>(&'a self, ctx: &ExtractCtx) -> &'a [(usize, Expr)] {
        let mut node = self;
        loop {
            match node {
                DecisionTree::Leaf(idxs) => return idxs,
                DecisionTree::Branch { tag, children, wildcard } => {
                    node = runtime::branch_key(ctx, tag).and_then(|v| children.get(v.as_ref())).unwrap_or(wildcard);
                }
                DecisionTree::AtomBranch { atom, on_true, on_false } => {
                    node = if runtime::eval_atom(atom, ctx) { on_true } else { on_false };
                }
            }
        }
    }
}

/// True iff `n` satisfies the single-bound comparison encoded by a `Predicate::Num`'s op + bit
/// pattern threshold. Shared by `kleene::decide_value`/`decide_wildcard` (build-time, partial) and
/// `runtime::eval_atom`/`decide_concrete` (runtime, total).
pub(crate) fn num_matches(op: &NumOp, threshold_bits: u64, n: f64) -> bool {
    let t = f64::from_bits(threshold_bits);
    match op {
        NumOp::Lt => n < t,
        NumOp::Lte => n <= t,
        NumOp::Gt => n > t,
        NumOp::Gte => n >= t,
    }
}
