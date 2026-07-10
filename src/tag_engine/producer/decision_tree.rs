//! Discrimination net over the compiled category `order`, to prune `categorize`'s first-match walk.
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
//!
//! This file holds the runtime half — the tree shape and `candidates()` walk. The build-time half
//! (compiling a `CategoriesFile`'s `order` into this tree) lives in `tag_engine::loader::decision_tree`.

use rustc_hash::FxHashMap;

use crate::tag_engine::producer::ExtractCtx;
use crate::lint::{NumOp, Predicate};

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

impl DecisionTree {
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
        SIDE_KEY => Some(ctx.obj_side),
        HAS_PARENT_KEY => Some(if ctx.parent_tags.is_some() { "true" } else { "false" }),
        PREFIX_KEY => ctx.prefix,
        INFIX_KEY => ctx.infix,
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
