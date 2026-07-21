//! Discrimination net over the compiled category `order`, to prune `categorize`'s first-match walk
//! — both the runtime tree shape/walk (`DecisionTree`, `candidates()`) and the load-time compiler
//! that builds one from a `CategoriesFile`'s compiled `order` (`build()`; nothing in that half runs
//! per-object).
//!
//! The tree branches on discriminating equality-tags (chiefly `highway`) and on a handful of
//! context fields that are *always* fully known and small-domain — `side`, `has_parent`, `prefix`,
//! `infix` — descending by the object's tag values / context to a leaf holding a small,
//! order-preserving subset of the priority list, then running the existing first-match `eval` over
//! just that subset.
//!
//! **Soundness.** The tree only prunes an order-node when it is *provably false* for the branch
//! value; the leaf's authoritative `eval` decides the rest. Pruning uses a three-valued (Kleene)
//! evaluation of the node's NNF condition under a partial assignment: for a branch on tag `T=v`,
//! every atom mentioning `T` is decided concretely (we know the value) and all other atoms are
//! `Unknown`. If the whole expression evaluates to `False`, the node cannot match → prune it. Worst
//! case the tree prunes nothing and matches the full walk; it never changes results.
//!
//! A branch key is a `BranchKey`: either a sentinel (`side`/`has_parent`/`prefix`/`infix`, reading
//! `ExtractCtx` fields rather than a tag) or a `Tag(Extract)` wrapping the same `Extract` its
//! candidate `Predicate`s carry — `Extract::read_str` already applies `sanitize` uniformly (see its
//! own doc), so branching on a sanitized comparison is exactly as sound as an unsanitized one; no
//! separate ban is needed.
//!
//! `side`/`has_parent`/`prefix`/`infix` branch on `ExtractCtx` fields rather than `ctx.obj_tags`,
//! using sentinel keys (`SIDE_KEY` etc. — a leading NUL byte no real OSM tag can contain) so they
//! reuse the same `Branch` shape and `used`/`choose_branch_tag` machinery as tag branching.
//!
//! `AtomBranch` handles the rest: `Contains`/`StartsWith`/`EndsWith`/`Num`/`Exists`/`HasKeyPrefix`
//! atoms are, unlike `Eq`, *total* — they evaluate to a concrete `bool` for every object, tag
//! present or not (a missing tag just makes `Contains` etc. false, mirroring `eval`'s semantics) —
//! so a single literal atom (e.g. `traffic_sign contains "237"`) can be used as its own two-way
//! branch with no wildcard/unknown case, decided by evaluating that one atom against the object.

use std::borrow::Cow;

use rustc_hash::{FxHashMap, FxHashSet};
use serde_json::Value;

use crate::categorize::categories::{CategoriesFile, OrderedNode};
use crate::lang::extract::Extract;
use crate::lang::filter::{read_num, Filter};
use crate::lang::producer::ExtractCtx;
use crate::categorize::linter::{filter_to_expr, to_nnf, Expr, Literal, NumOp, Predicate};

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
                if sanitize.is_some() { write!(f, " (sanitized)")?; }
                Ok(())
            }
            BranchKey::Tag(Extract::Candidates { keys, sanitize }) => {
                write!(f, "{}", keys.join("/"))?;
                if sanitize.is_some() { write!(f, " (sanitized)")?; }
                Ok(())
            }
        }
    }
}

#[derive(Debug, Clone)]
pub enum DecisionTree {
    /// Order-node index, paired with its condition as simplified along the path to this leaf (see
    /// `Candidate`'s own doc), in ascending (= priority) order. Each pair's `Expr` already has every
    /// fact the branches above proved baked in — `eval_expr` (not the original `Filter`) is what a
    /// leaf walk should evaluate, so it never re-reads/re-compares a tag the tree already decided.
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
                    node = branch_key(ctx, tag).and_then(|v| children.get(v.as_ref())).unwrap_or(wildcard);
                }
                DecisionTree::AtomBranch { atom, on_true, on_false } => {
                    node = if eval_atom(atom, ctx) { on_true } else { on_false };
                }
            }
        }
    }
}

/// Evaluate a single atom directly against the object's context. Only called for the atom kinds
/// `AtomBranch` is ever built from (`Contains`/`StartsWith`/`EndsWith`/`Num`/`Exists`/`HasKeyPrefix`
/// on a plain, non-parent tag) — all total functions of `ctx.tags`, mirroring `eval`'s semantics
/// for a missing tag (false, not unknown).
fn eval_atom(atom: &Predicate, ctx: &ExtractCtx) -> bool {
    match atom {
        Predicate::Contains(e, s) => e.read_str(ctx).is_some_and(|v| v.contains(s.as_str())),
        Predicate::StartsWith(e, s) => e.read_str(ctx).is_some_and(|v| v.starts_with(s.as_str())),
        Predicate::EndsWith(e, s) => e.read_str(ctx).is_some_and(|v| v.ends_with(s.as_str())),
        Predicate::Exists(e) => e.read_str(ctx).is_some(),
        Predicate::Num(e, op, bits) => read_num(e, ctx).is_some_and(|n| num_matches(op, *bits, n)),
        Predicate::HasKeyPrefix(p) => ctx.obj_tags.keys().any(|k| k.starts_with(p.as_str())),
        _ => unreachable!("AtomBranch only built for Contains/StartsWith/EndsWith/Exists/Num/HasKeyPrefix"),
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

/// Fully decide one predicate against a concrete object — unlike `decide_value`/`decide_wildcard`
/// (partial, build-time, three-valued under one known fact), every predicate is decidable here since
/// `ctx` is a real object. Mirrors `lang::filter::eval`'s per-`Filter`-variant semantics exactly (see
/// each arm), since this is what stands in for a full `Filter` re-eval at leaf time.
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
/// `kleene`/`simplify` (which only ever partially decide, under one just-branched fact). Every
/// `Predicate` is decidable here, so this collapses straight to a `bool`, no `Unknown` case.
pub(crate) fn eval_expr(e: &Expr, ctx: &ExtractCtx) -> bool {
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

/// Resolve a branch key against the object's context. `None` means "no matching enumerated child"
/// → fall through to the wildcard.
fn branch_key<'a>(ctx: &'a ExtractCtx, key: &BranchKey) -> Option<Cow<'a, str>> {
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
        BranchKey::Tag(extract) => extract.read_str(ctx),
    }
}

/// True iff `n` satisfies the single-bound comparison encoded by a `Predicate::Num`'s op + bit
/// pattern threshold.
pub(crate) fn num_matches(op: &NumOp, threshold_bits: u64, n: f64) -> bool {
    let t = f64::from_bits(threshold_bits);
    match op {
        NumOp::Lt => n < t,
        NumOp::Lte => n <= t,
        NumOp::Gt => n > t,
        NumOp::Gte => n >= t,
    }
}

// ── Load-time compiler ──────────────────────────────────────────────────────────
//
// Builds a `DecisionTree` from a topic's compiled `order` (+ categories/macros for conditions).
// Nothing below this point runs per-object.

/// Stop branching once a candidate set shrinks to this size — below this, the eval cost of a leaf
/// walk is already cheap, so a further branch's overhead isn't worth the diminishing prune.
const LEAF_MAX: usize = 2;

/// An order-node index paired with two expressions, both simplified so far along this build path
/// (see `simplify`): `residual` is `cond_i ∧ ¬cond_1 ∧ … ∧ ¬cond_{i-1}` (first-match's own effective
/// condition — see `build`'s doc) and is what pruning/branch-choice reasons about, so a node
/// deducible-dead *relative to earlier nodes* still gets eliminated; `own` is just `cond_i` alone,
/// with the same branch facts folded in, and is what a surviving leaf candidate actually needs at
/// eval time — first-match is already handled for free by a leaf's own (index-ascending, first-hit-
/// wins) candidate order, the same way `categorize_linear`'s loop gets it from iteration order alone,
/// so `own` never needs the `¬cond_1…` conjuncts `residual` carries only for *this* build's pruning.
/// Both fold in lockstep off the same just-decided fact (see `fold_candidates`), so `own` stays
/// exactly as reduced as `residual` — just without the redundant-at-eval-time prior negations.
type Candidate = (usize, Expr, Expr);

/// Build the discrimination net from a topic's compiled `order` (+ categories/macros for
/// conditions). `max_depth` caps how many tags deep a branch chain may go (see `build_rec`).
pub fn build(cats: &CategoriesFile, max_depth: usize) -> DecisionTree {
    // NNF condition per order node (index-aligned with `cats.order`).
    let exprs: Vec<Expr> = cats
        .order
        .iter()
        .map(|n| to_nnf(filter_to_expr(node_condition(cats, n))))
        .collect();

    // First-match semantics mean node `i` only ever decides anything once every earlier node has
    // failed, so its *effective* condition is `cond_i ∧ ¬cond_1 ∧ … ∧ ¬cond_{i-1}` — fold that in
    // once, here, at load time. This both hands the tree builder a smaller residual to branch on
    // and, when a node's residual collapses to `Expr::False` outright, proves it's fully shadowed
    // (dead) before the tree is even built.
    let neg_exprs: Vec<Expr> =
        exprs.iter().map(|e| to_nnf(Expr::Not(Box::new(e.clone())))).collect();
    let mut residuals: Vec<Expr> = Vec::with_capacity(exprs.len());
    for i in 0..exprs.len() {
        let mut parts = Vec::with_capacity(i + 1);
        parts.push(exprs[i].clone());
        parts.extend(neg_exprs[..i].iter().cloned());
        residuals.push(conjoin(parts));
    }

    let all: Vec<Candidate> = residuals.into_iter().zip(exprs).enumerate()
        .map(|(i, (residual, own))| (i, residual, own))
        .collect();
    build_rec(all, &mut FxHashSet::default(), &mut FxHashSet::default(), 0, max_depth)
}

/// Build an `And` of `parts` (each already NNF), flattening nested `And`s and collapsing to
/// `Expr::False` when two direct top-level conjuncts are exact-opposite literals of the same
/// predicate. Doesn't attempt full DNF-level contradiction detection (that's `to_dnf` +
/// `check_term_consistency`, used by the overlap lint): expanding to DNF here risks exponential
/// blowup once many prior `Or`-shaped conditions are negated and ANDed together, so this only
/// catches contradictions between literals that are already top-level conjuncts, not ones buried
/// across `Or` branches — the rest of a node's residual complexity gets resolved lazily, per
/// branch, by `simplify`/`kleene` during the tree walk below.
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

fn node_condition<'a>(cats: &'a CategoriesFile, n: &'a OrderedNode) -> &'a Filter {
    match n {
        OrderedNode::Category { idx } => &cats.categories[*idx].condition,
        OrderedNode::Skip { condition } => condition,
    }
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

            let mut children = FxHashMap::default();
            for v in &values {
                let kept = fold_candidates(&candidates, &|p| decide_value(p, &tag, v));
                children.insert(
                    v.clone(),
                    build_rec(kept, used, used_atoms, depth + 1, max_depth),
                );
            }
            let wild = fold_candidates(&candidates, &|p| decide_wildcard(p, &tag, &values));
            let wildcard = Box::new(build_rec(wild, used, used_atoms, depth + 1, max_depth));

            used.remove(&tag);
            DecisionTree::Branch { tag, children, wildcard }
        }
        BranchChoice::Atom(atom) => {
            used_atoms.insert(atom.clone());

            let b = |cond: bool| if cond { K::T } else { K::F };
            let on_true = fold_candidates(&candidates, &|p| if p == &atom { b(true) } else { K::U });
            let on_false = fold_candidates(&candidates, &|p| if p == &atom { b(false) } else { K::U });
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

/// A branch key naming a plain (non-parent-scoped) tag isn't eligible if any of its `Extract`'s own
/// key(s) is itself parent-scoped (the synthetic `parent_<key>` name `prefix_expr_tags` stamps for
/// a `Filter::Parent` condition) — there's no such tag in `ctx.obj_tags` to read. Matches directly
/// on `Extract` rather than going through `tag_names()` — this runs on every predicate decision in
/// `eval_expr`'s per-object hot path (via `read_extract`/`read_extract_num`), so it can't afford
/// `tag_names()`'s `Vec<String>` allocation for what's almost always a `false` answer.
fn parent_scoped(extract: &Extract) -> bool {
    match extract {
        Extract::Value { key, .. } => key.starts_with("parent_"),
        Extract::Candidates { keys, .. } => keys.iter().any(|k| k.starts_with("parent_")),
    }
}

/// Pick the branch (tag/context key, or single atom) whose worst-case child is smallest, provided
/// it prunes at all. `None` if nothing reduces the candidate set.
fn choose_branch(
    candidates: &[Candidate],
    used: &FxHashSet<BranchKey>,
    used_atoms: &FxHashSet<Predicate>,
) -> Option<BranchChoice> {
    let mut best: Option<BranchChoice> = None;
    let mut best_worst = candidates.len(); // require strictly smaller than the full set

    // Candidate branch keys: tags (plain or sanitized) appearing in a positive Eq/FirstTagIn atom,
    // plus the `side`/`has_parent`/`prefix`/`infix` sentinels wherever those predicates appear at
    // all.
    let mut tags: FxHashSet<BranchKey> = FxHashSet::default();
    for (_, e, _) in candidates {
        collect_branch_keys(e, &mut |k| {
            let parent_scoped = match &k {
                BranchKey::Tag(extract) => parent_scoped(extract),
                BranchKey::Sentinel(_) => false,
            };
            if !parent_scoped && !used.contains(&k) {
                tags.insert(k);
            }
        });
    }
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

    // Candidate atoms: total (non-Eq) predicates on a plain, non-parent-scoped tag — `Contains`,
    // `StartsWith`, `EndsWith`, `Num`, `Exists`, `HasKeyPrefix` — either polarity, since a missing
    // tag decides them outright (false) rather than leaving them unknown.
    let mut atoms: FxHashSet<Predicate> = FxHashSet::default();
    for (_, e, _) in candidates {
        collect_atom_candidates(e, &mut |p| {
            if !used_atoms.contains(p) {
                atoms.insert(p.clone());
            }
        });
    }
    for atom in atoms {
        let b = |cond: bool| if cond { K::T } else { K::F };
        let t = candidates
            .iter()
            .filter(|(_, e, _)| keep_for(e, &|p| if p == &atom { b(true) } else { K::U }))
            .count();
        let f = candidates
            .iter()
            .filter(|(_, e, _)| keep_for(e, &|p| if p == &atom { b(false) } else { K::U }))
            .count();
        let worst = t.max(f);
        if worst < best_worst {
            best_worst = worst;
            best = Some(BranchChoice::Atom(atom));
        }
    }

    best
}

/// All exact-eq values of `key` across the candidate conditions. For the `side`/`has_parent`
/// sentinels the domain is fixed and known up front (no need to scan conditions for it).
fn eligible_values(candidates: &[Candidate], key: &BranchKey) -> Vec<String> {
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

fn keep_for(expr: &Expr, decide: &impl Fn(&Predicate) -> K) -> bool {
    kleene(expr, decide) != K::F
}

// ── Three-valued (Kleene) evaluation ─────────────────────────────────────────────

#[derive(PartialEq, Eq, Clone, Copy)]
enum K {
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

fn kleene(e: &Expr, decide: &impl Fn(&Predicate) -> K) -> K {
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

/// Constant-fold `e` under `decide`: replace every literal `decide` can resolve with `True`/`False`
/// and normalize `Not`/`And`/`Or` (an `And` with a `False` child collapses to `False`, an `Or` with
/// a `True` child collapses to `True`, singleton `And`/`Or` unwrap). This is what lets several
/// branches jointly resolve an `Or` none of them could decide alone: each branch's fact gets baked
/// into the candidate's expression, so it's already gone by the time a later branch or leaf looks.
fn simplify(e: &Expr, decide: &impl Fn(&Predicate) -> K) -> Expr {
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
fn decide_value(p: &Predicate, key: &BranchKey, v: &str) -> K {
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
/// (`eligible_values`'s result for this same branch)". Only a target value that's actually in
/// `values` can be folded to `False` here — `values` is collected from *positive* Eq/FirstTagIn/
/// Prefix/Infix occurrences only (`collect_eq_values`), so a negative-only literal naming some other
/// value was never enumerated as a child, and the wildcard object could still carry exactly that
/// value; folding it to `False` regardless (as if every nameable value were enumerated) is unsound —
/// it doesn't just miss a prune, it can make the leaf's residual `Expr` claim a fact that isn't true
/// (`eval_expr` would then trust it). Existence/value-shape atoms stay `Unknown` either way (the
/// object could still carry an un-enumerated value with that shape).
fn decide_wildcard(p: &Predicate, key: &BranchKey, values: &[String]) -> K {
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

// ── Expr walkers ─────────────────────────────────────────────────────────────────

/// Invoke `f` on the `Extract` of every positive `Eq`/`FirstTagIn` atom, and on the relevant
/// sentinel key for every `Side`/`HasParent`/`Prefix`/`Infix` atom (either polarity — their fixed,
/// small domain makes them decidable regardless of sign, unlike an arbitrary-string tag `Eq`/`Neq`).
fn collect_branch_keys(e: &Expr, f: &mut impl FnMut(BranchKey)) {
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
/// polarity) whose tag (if any) isn't parent-scoped — the atom kinds `AtomBranch` can use.
fn collect_atom_candidates(e: &Expr, f: &mut impl FnMut(&Predicate)) {
    match e {
        Expr::Lit(Literal::Pos(p) | Literal::Neg(p)) => {
            let extract = match p {
                Predicate::Contains(e, _)
                | Predicate::StartsWith(e, _)
                | Predicate::EndsWith(e, _)
                | Predicate::Exists(e)
                | Predicate::Num(e, _, _) => Some(e),
                Predicate::HasKeyPrefix(_) => None,
                _ => return,
            };
            if let Some(extract) = extract {
                if parent_scoped(extract) {
                    return;
                }
            }
            f(p);
        }
        Expr::True | Expr::False => {}
        Expr::Not(x) => collect_atom_candidates(x, f),
        Expr::And(xs) | Expr::Or(xs) => xs.iter().for_each(|x| collect_atom_candidates(x, f)),
    }
}

/// Invoke `f` on the value(s) of every positive `Eq`/`FirstTagIn` atom on `key`'s `Extract` (or, for
/// `PREFIX_KEY`/`INFIX_KEY`, every positive `Prefix`/`Infix` atom's literal value).
fn collect_eq_values(e: &Expr, key: &BranchKey, f: &mut impl FnMut(&str)) {
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

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet, HashMap};

    use crate::topic::load::{
        load_shared_macros, load_topic_categories, load_topic_macros, load_topic_sanitizers, merge, resolve_macros,
    };
    use crate::categorize::categories::{categorize, categorize_linear, CategoriesFile, OrderedNode};
    use crate::lang::filter::Filter;
    use crate::lang::producer::ExtractCtx;
    use crate::categorize::linter::{filter_to_expr, to_nnf, topic_category_dirs, Expr, Literal, Predicate};
    use serde_json::{Map, Value};
    use crate::osm::types::RawTags;

    /// Positive Eq (plain-tag) atoms across every order-node condition → tag → observed values.
    fn referenced_eq(cats: &CategoriesFile) -> BTreeMap<String, BTreeSet<String>> {
        let mut out: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for n in &cats.order {
            let cond = match n {
                OrderedNode::Category { idx } => &cats.categories[*idx].condition,
                OrderedNode::Skip { condition } => condition,
            };
            let e = to_nnf(filter_to_expr(cond));
            collect_pairs(&e, &mut out);
        }
        out
    }

    fn collect_pairs(e: &Expr, out: &mut BTreeMap<String, BTreeSet<String>>) {
        match e {
            Expr::Lit(Literal::Pos(Predicate::Eq(extract, v))) => {
                for k in extract.tag_names().into_iter().filter(|k| !k.starts_with("parent_")) {
                    out.entry(k).or_default().insert(v.clone());
                }
            }
            Expr::Lit(_) | Expr::True | Expr::False => {}
            Expr::Not(x) => collect_pairs(x, out),
            Expr::And(xs) | Expr::Or(xs) => xs.iter().for_each(|x| collect_pairs(x, out)),
        }
    }

    /// The tree-pruned `categorize` must return the same category as the full linear walk for every
    /// object we can construct — the tree only drops provably-false nodes.
    #[test]
    fn tree_matches_linear() {
        for (topic, dir) in topic_category_dirs() {
          let config_root = dir.parent().unwrap();
          let shared_macros = load_shared_macros(config_root).expect("shared macros");
          let sanitizers = load_topic_sanitizers(&dir, config_root).expect("load sanitizers");
          let topic_macros = load_topic_macros(&dir).expect("topic macros");
          let raw_macros = merge(&shared_macros, &topic_macros);
          let resolved_macros = resolve_macros(&raw_macros, &sanitizers).expect("resolve macros");
          let macros: HashMap<String, Filter> = resolved_macros.iter()
              .map(|(k, v)| Ok((k.clone(), serde_json::from_value(v.clone())?)))
              .collect::<anyhow::Result<_>>()
              .expect("parse resolved macros");
          for (kind, mut cats) in load_topic_categories(&dir, &resolved_macros, &macros, &sanitizers).expect("load categories") {
            let topic = format!("{topic}/{}", kind.subdir());
            cats.build_order(crate::config::DEFAULT_TREE_MAX_DEPTH).expect("build order + tree");

            let refs = referenced_eq(&cats);
            let hw_vals: Vec<Option<String>> = {
                let mut v: Vec<Option<String>> =
                    refs.get("highway").into_iter().flatten().map(|s| Some(s.clone())).collect();
                v.push(Some("zzz_unknown".to_string())); // an un-enumerated value → wildcard
                v.push(None); // highway absent
                v
            };
            let mut others: Vec<Option<(String, String)>> = vec![None];
            for (t, vals) in &refs {
                if t == "highway" {
                    continue;
                }
                for val in vals {
                    others.push(Some((t.clone(), val.clone())));
                }
            }

            let sides = ["self", "left", "right"];
            let parent_tags: RawTags =
                [("highway".to_owned(), "secondary".to_owned())].into_iter().collect();
            let mut checked = 0usize;
            for hw in &hw_vals {
                for other in &others {
                    for &side in &sides {
                        let mut tags: RawTags = RawTags::default();
                        if let Some(h) = hw {
                            tags.insert("highway".into(), h.clone());
                        }
                        if let Some((t, v)) = other {
                            tags.insert(t.clone(), v.clone());
                        }
                        let (prefix, side_parent_tags): (Option<&str>, Option<&RawTags>) = match side {
                            "self" => (None, None),
                            _ => (Some("cycleway"), Some(&parent_tags)),
                        };
                        let mut annotations = Map::new();
                        annotations.insert("_side".to_owned(), Value::String(side.to_owned()));
                        if let Some(p) = prefix {
                            annotations.insert("_prefix".to_owned(), Value::String(p.to_owned()));
                        }
                        let ctx = ExtractCtx {
                            obj_tags: &tags,
                            parent_tags: side_parent_tags,
                            id: "",
                            annotations: &annotations,
                        };
                        let a = categorize(&ctx, &cats).map(|c| c.id.clone());
                        let b = categorize_linear(&ctx, &cats).map(|c| c.id.clone());
                        assert_eq!(
                            a, b,
                            "[{topic}] tree≠linear for highway={hw:?} other={other:?} side={side:?}"
                        );
                        checked += 1;
                    }
                }
            }
            assert!(checked > 0, "[{topic}] no test cases generated");
          }
        }
    }
}

