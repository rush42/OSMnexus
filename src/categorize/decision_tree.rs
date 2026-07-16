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
//! Only sanitize-free tags are branch keys: a sanitized comparison tests a *derived* value, so
//! branching on the raw tag value would be unsound (and `filter_to_expr` drops the sanitize flag).
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

use rustc_hash::{FxHashMap, FxHashSet};
use serde_json::Value;

use crate::categorize::categories::{CategoriesFile, OrderedNode};
use crate::lang::filter::Filter;
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

#[derive(Debug, Clone)]
pub enum DecisionTree {
    /// Order-node indices (ascending = priority order) to first-match `eval` over.
    Leaf(Vec<usize>),
    Branch {
        tag: String,
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

    /// Descend to the leaf's candidate node-index slice for this object.
    pub fn candidates<'a>(&'a self, ctx: &ExtractCtx) -> &'a [usize] {
        let mut node = self;
        loop {
            match node {
                DecisionTree::Leaf(idxs) => return idxs,
                DecisionTree::Branch { tag, children, wildcard } => {
                    node = branch_key(ctx, tag).and_then(|v| children.get(v)).unwrap_or(wildcard);
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
        Predicate::Contains(k, s) => ctx.obj_tags.get(k).is_some_and(|v| v.contains(s.as_str())),
        Predicate::StartsWith(k, s) => ctx.obj_tags.get(k).is_some_and(|v| v.starts_with(s.as_str())),
        Predicate::EndsWith(k, s) => ctx.obj_tags.get(k).is_some_and(|v| v.ends_with(s.as_str())),
        Predicate::Exists(k) => ctx.obj_tags.contains_key(k),
        Predicate::Num(k, op, bits) => ctx
            .obj_tags
            .get(k)
            .and_then(|v| v.trim().parse::<f64>().ok())
            .is_some_and(|n| num_cmp(n, op, f64::from_bits(*bits))),
        Predicate::HasKeyPrefix(p) => ctx.obj_tags.keys().any(|k| k.starts_with(p.as_str())),
        _ => unreachable!("AtomBranch only built for Contains/StartsWith/EndsWith/Exists/Num/HasKeyPrefix"),
    }
}

/// Resolve a branch key (a raw tag name, or one of the `*_KEY` sentinels) against the object's
/// context. `None` means "no matching enumerated child" → fall through to the wildcard.
fn branch_key<'a>(ctx: &'a ExtractCtx, tag: &str) -> Option<&'a str> {
    match tag {
        // `_side` is always stamped by `topic::pipeline::build_topic_rows`/each `Clone` (self
        // included) — defaulting to "self" here is a safety net, not the normal path.
        SIDE_KEY => Some(ctx.annotations.get("_side").and_then(Value::as_str).unwrap_or("self")),
        HAS_PARENT_KEY => Some(if ctx.parent_tags.is_some() { "true" } else { "false" }),
        PREFIX_KEY => ctx.annotations.get("_prefix").and_then(Value::as_str),
        INFIX_KEY => ctx.annotations.get("_infix").and_then(Value::as_str),
        _ => ctx.obj_tags.get(tag).map(String::as_str),
    }
}

pub(crate) fn num_cmp(n: f64, op: &NumOp, thr: f64) -> bool {
    match op {
        NumOp::Lt => n < thr,
        NumOp::Lte => n <= thr,
        NumOp::Gt => n > thr,
        NumOp::Gte => n >= thr,
    }
}

// ── Load-time compiler ──────────────────────────────────────────────────────────
//
// Builds a `DecisionTree` from a topic's compiled `order` (+ categories/macros for conditions).
// Nothing below this point runs per-object.

/// Stop branching once a candidate set shrinks to this size — below this, the eval cost of a leaf
/// walk is already cheap, so a further branch's overhead isn't worth the diminishing prune.
const LEAF_MAX: usize = 2;

/// An order-node index paired with its condition *as simplified so far* along this build path —
/// each branch constant-folds the just-decided fact into every surviving candidate's expression
/// (see `simplify`), so later branches (and the leaf) see a strictly smaller residual, and several
/// branches can jointly resolve an `Or` that no single one of them could decide alone.
type Candidate = (usize, Expr);

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

    // Tags used anywhere with a `sanitize` chain are ineligible as branch keys.
    let mut banned: FxHashSet<String> = FxHashSet::default();
    for n in &cats.order {
        collect_sanitized_tags(node_condition(cats, n), &mut banned);
    }

    let all: Vec<Candidate> = residuals.into_iter().enumerate().collect();
    build_rec(&banned, all, &mut FxHashSet::default(), &mut FxHashSet::default(), 0, max_depth)
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
    banned: &FxHashSet<String>,
    candidates: Vec<Candidate>,
    used: &mut FxHashSet<String>,
    used_atoms: &mut FxHashSet<Predicate>,
    depth: usize,
    max_depth: usize,
) -> DecisionTree {
    if candidates.len() <= LEAF_MAX || depth >= max_depth {
        return DecisionTree::Leaf(candidates.into_iter().map(|(i, _)| i).collect());
    }
    let Some(choice) = choose_branch(banned, &candidates, used, used_atoms) else {
        return DecisionTree::Leaf(candidates.into_iter().map(|(i, _)| i).collect());
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
                    build_rec(banned, kept, used, used_atoms, depth + 1, max_depth),
                );
            }
            let wild = fold_candidates(&candidates, &|p| decide_wildcard(p, &tag));
            let wildcard = Box::new(build_rec(banned, wild, used, used_atoms, depth + 1, max_depth));

            used.remove(&tag);
            DecisionTree::Branch { tag, children, wildcard }
        }
        BranchChoice::Atom(atom) => {
            used_atoms.insert(atom.clone());

            let b = |cond: bool| if cond { K::T } else { K::F };
            let on_true = fold_candidates(&candidates, &|p| if p == &atom { b(true) } else { K::U });
            let on_false = fold_candidates(&candidates, &|p| if p == &atom { b(false) } else { K::U });
            let on_true = Box::new(build_rec(banned, on_true, used, used_atoms, depth + 1, max_depth));
            let on_false =
                Box::new(build_rec(banned, on_false, used, used_atoms, depth + 1, max_depth));

            used_atoms.remove(&atom);
            DecisionTree::AtomBranch { atom, on_true, on_false }
        }
    }
}

/// Constant-fold every candidate's current expression under `decide`, dropping any that collapse
/// to `Expr::False` and keeping the (possibly smaller) simplified expression for the survivors.
fn fold_candidates(candidates: &[Candidate], decide: &impl Fn(&Predicate) -> K) -> Vec<Candidate> {
    candidates
        .iter()
        .filter_map(|(i, e)| {
            let simplified = simplify(e, decide);
            (simplified != Expr::False).then_some((*i, simplified))
        })
        .collect()
}

enum BranchChoice {
    Key(String),
    Atom(Predicate),
}

/// Pick the branch (tag/context key, or single atom) whose worst-case child is smallest, provided
/// it prunes at all. `None` if nothing reduces the candidate set.
fn choose_branch(
    banned: &FxHashSet<String>,
    candidates: &[Candidate],
    used: &FxHashSet<String>,
    used_atoms: &FxHashSet<Predicate>,
) -> Option<BranchChoice> {
    let mut best: Option<BranchChoice> = None;
    let mut best_worst = candidates.len(); // require strictly smaller than the full set

    // Candidate branch keys: plain (non-parent) tags appearing in a positive Eq atom, plus the
    // `side`/`has_parent`/`prefix`/`infix` sentinels wherever those predicates appear at all.
    let mut tags: FxHashSet<String> = FxHashSet::default();
    for (_, e) in candidates {
        collect_branch_keys(e, &mut |k| {
            if !k.starts_with("parent_") && !banned.contains(k) && !used.contains(k) {
                tags.insert(k.to_string());
            }
        });
    }
    for tag in tags {
        let values = eligible_values(candidates, &tag);
        let mut worst =
            candidates.iter().filter(|(_, e)| keep_for(e, &|p| decide_wildcard(p, &tag))).count();
        for v in &values {
            let c = candidates.iter().filter(|(_, e)| keep_for(e, &|p| decide_value(p, &tag, v))).count();
            worst = worst.max(c);
        }
        if worst < best_worst {
            best_worst = worst;
            best = Some(BranchChoice::Key(tag));
        }
    }

    // Candidate atoms: total (non-Eq) predicates on a plain, unsanitized tag — `Contains`,
    // `StartsWith`, `EndsWith`, `Num`, `Exists`, `HasKeyPrefix` — either polarity, since a missing
    // tag decides them outright (false) rather than leaving them unknown.
    let mut atoms: FxHashSet<Predicate> = FxHashSet::default();
    for (_, e) in candidates {
        collect_atom_candidates(e, banned, &mut |p| {
            if !used_atoms.contains(p) {
                atoms.insert(p.clone());
            }
        });
    }
    for atom in atoms {
        let b = |cond: bool| if cond { K::T } else { K::F };
        let t = candidates
            .iter()
            .filter(|(_, e)| keep_for(e, &|p| if p == &atom { b(true) } else { K::U }))
            .count();
        let f = candidates
            .iter()
            .filter(|(_, e)| keep_for(e, &|p| if p == &atom { b(false) } else { K::U }))
            .count();
        let worst = t.max(f);
        if worst < best_worst {
            best_worst = worst;
            best = Some(BranchChoice::Atom(atom));
        }
    }

    best
}

/// All exact-eq values of `tag` across the candidate conditions. For the `side`/`has_parent`
/// sentinels the domain is fixed and known up front (no need to scan conditions for it).
fn eligible_values(candidates: &[Candidate], tag: &str) -> Vec<String> {
    match tag {
        SIDE_KEY => return vec!["left".to_string(), "right".to_string(), "self".to_string()],
        HAS_PARENT_KEY => return vec!["false".to_string(), "true".to_string()],
        _ => {}
    }
    let mut vals: FxHashSet<String> = FxHashSet::default();
    for (_, e) in candidates {
        collect_eq_values(e, tag, &mut |v| {
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

/// Decide a predicate under "object's `tag` == `v`, everything else unknown". Atoms on `tag` are
/// fully decidable (we know the value); all others are `Unknown`.
fn decide_value(p: &Predicate, tag: &str, v: &str) -> K {
    let b = |cond: bool| if cond { K::T } else { K::F };
    match tag {
        SIDE_KEY => return match p { Predicate::Side(s) => b(s == v), _ => K::U },
        HAS_PARENT_KEY => return match p { Predicate::HasParent => b(v == "true"), _ => K::U },
        PREFIX_KEY => return match p { Predicate::Prefix(s) => b(s == v), _ => K::U },
        INFIX_KEY => return match p { Predicate::Infix(s) => b(s == v), _ => K::U },
        _ => {}
    }
    match p {
        Predicate::Eq(k, w) if k == tag => b(w == v),
        Predicate::Exists(k) if k == tag => K::T,
        Predicate::Contains(k, s) if k == tag => b(v.contains(s.as_str())),
        Predicate::StartsWith(k, s) if k == tag => b(v.starts_with(s.as_str())),
        Predicate::EndsWith(k, s) if k == tag => b(v.ends_with(s.as_str())),
        Predicate::Num(k, op, bits) if k == tag => match v.trim().parse::<f64>() {
            Ok(n) => b(num_cmp(n, op, f64::from_bits(*bits))),
            Err(_) => K::F,
        },
        _ => K::U,
    }
}

/// Decide a predicate under "object's `tag` is absent or not among the enumerated eq-values".
/// Every positive Eq on `tag` in the candidate set uses an enumerated value, so all are false here;
/// existence and value-shape atoms stay unknown (the object could carry an un-enumerated value).
fn decide_wildcard(p: &Predicate, tag: &str) -> K {
    match tag {
        SIDE_KEY => return match p { Predicate::Side(_) => K::F, _ => K::U },
        HAS_PARENT_KEY => return match p { Predicate::HasParent => K::F, _ => K::U },
        PREFIX_KEY => return match p { Predicate::Prefix(_) => K::F, _ => K::U },
        INFIX_KEY => return match p { Predicate::Infix(_) => K::F, _ => K::U },
        _ => {}
    }
    match p {
        Predicate::Eq(k, _) if k == tag => K::F,
        _ => K::U,
    }
}

// ── Expr walkers ─────────────────────────────────────────────────────────────────

/// Invoke `f` on the key of every positive `Eq` atom, and on the relevant sentinel key for every
/// `Side`/`HasParent`/`Prefix`/`Infix` atom (either polarity — their fixed, small domain makes them
/// decidable regardless of sign, unlike an arbitrary-string tag `Eq`/`Neq`).
fn collect_branch_keys(e: &Expr, f: &mut impl FnMut(&str)) {
    match e {
        Expr::Lit(Literal::Pos(Predicate::Eq(k, _))) => f(k),
        Expr::Lit(Literal::Pos(Predicate::Side(_)) | Literal::Neg(Predicate::Side(_))) => f(SIDE_KEY),
        Expr::Lit(Literal::Pos(Predicate::HasParent) | Literal::Neg(Predicate::HasParent)) => {
            f(HAS_PARENT_KEY)
        }
        Expr::Lit(Literal::Pos(Predicate::Prefix(_)) | Literal::Neg(Predicate::Prefix(_))) => {
            f(PREFIX_KEY)
        }
        Expr::Lit(Literal::Pos(Predicate::Infix(_)) | Literal::Neg(Predicate::Infix(_))) => {
            f(INFIX_KEY)
        }
        Expr::Lit(_) | Expr::True | Expr::False => {}
        Expr::Not(x) => collect_branch_keys(x, f),
        Expr::And(xs) | Expr::Or(xs) => xs.iter().for_each(|x| collect_branch_keys(x, f)),
    }
}

/// Invoke `f` on every `Contains`/`StartsWith`/`EndsWith`/`Num`/`Exists`/`HasKeyPrefix` atom (either
/// polarity) whose tag (if any) is plain and unsanitized — the atom kinds `AtomBranch` can use.
fn collect_atom_candidates(e: &Expr, banned: &FxHashSet<String>, f: &mut impl FnMut(&Predicate)) {
    match e {
        Expr::Lit(Literal::Pos(p) | Literal::Neg(p)) => {
            let tag = match p {
                Predicate::Contains(k, _)
                | Predicate::StartsWith(k, _)
                | Predicate::EndsWith(k, _)
                | Predicate::Exists(k)
                | Predicate::Num(k, _, _) => Some(k.as_str()),
                Predicate::HasKeyPrefix(_) => None,
                _ => return,
            };
            if let Some(tag) = tag {
                if tag.starts_with("parent_") || banned.contains(tag) {
                    return;
                }
            }
            f(p);
        }
        Expr::True | Expr::False => {}
        Expr::Not(x) => collect_atom_candidates(x, banned, f),
        Expr::And(xs) | Expr::Or(xs) => xs.iter().for_each(|x| collect_atom_candidates(x, banned, f)),
    }
}

/// Invoke `f` on the value of every positive `Eq` atom on `tag` (or, for `PREFIX_KEY`/`INFIX_KEY`,
/// every positive `Prefix`/`Infix` atom's literal value).
fn collect_eq_values(e: &Expr, tag: &str, f: &mut impl FnMut(&str)) {
    match e {
        Expr::Lit(Literal::Pos(Predicate::Eq(k, v))) if k == tag => f(v),
        Expr::Lit(Literal::Pos(Predicate::Prefix(v))) if tag == PREFIX_KEY => f(v),
        Expr::Lit(Literal::Pos(Predicate::Infix(v))) if tag == INFIX_KEY => f(v),
        Expr::Lit(_) | Expr::True | Expr::False => {}
        Expr::Not(x) => collect_eq_values(x, tag, f),
        Expr::And(xs) | Expr::Or(xs) => xs.iter().for_each(|x| collect_eq_values(x, tag, f)),
    }
}

/// Collect plain tag names compared through a `sanitize` chain (recursing into macros). Such tags
/// are ineligible as branch keys — the comparison tests a derived value, not the raw tag.
fn collect_sanitized_tags(f: &Filter, out: &mut FxHashSet<String>) {
    match f {
        Filter::And { and } => and.iter().for_each(|c| collect_sanitized_tags(c, out)),
        Filter::Or { or } => or.iter().for_each(|c| collect_sanitized_tags(c, out)),
        Filter::Not { not } => collect_sanitized_tags(not, out),
        Filter::Eq { extract, .. }
        | Filter::In { extract, .. }
        | Filter::NumLt { extract, .. }
        | Filter::NumLte { extract, .. }
        | Filter::NumGt { extract, .. }
        | Filter::NumGte { extract, .. } => {
            if extract.sanitize().is_some() {
                if let crate::lang::extract::Extract::Value { key, .. } = extract {
                    out.insert(key.clone());
                }
            }
        }
        _ => {}
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
            Expr::Lit(Literal::Pos(Predicate::Eq(k, v))) if !k.starts_with("parent_") => {
                out.entry(k.clone()).or_default().insert(v.clone());
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

