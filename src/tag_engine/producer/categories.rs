use std::collections::HashMap;

use serde::Deserialize;

use crate::tag_engine::loader::topic::DeriverBinding;
use crate::tag_engine::producer::decision_tree::DecisionTree;
use crate::tag_engine::producer::filter::{eval, Filter};
use crate::tag_engine::producer::ExtractCtx;

// ── Data types ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
pub struct CategoryDef {
    pub id: String,
    pub condition: Filter,
    pub excludes: Option<Vec<String>>,
    /// Per-category deriver overrides: re-bind a different deriver to an output (replacing the
    /// topic default for that output). E.g. surface/smoothness sourced from the parent highway.
    #[serde(default)]
    pub derivers: Option<Vec<DeriverBinding>>,
    /// Per-category constants (override the topic-level `consts` per key). Seeded into `derived`
    /// as the lowest-priority layer; a sanitizer/deriver producing the same key overrides them.
    #[serde(default)]
    pub consts: serde_json::Map<String, serde_json::Value>,
    /// Per-category private metadata (override the topic-level `private` per key). Emitted into the
    /// `private` output column verbatim — the explicit counterpart to `consts`, for internal keys
    /// like `_implicit_oneway_confidence` that are not part of the public `derived` payload.
    #[serde(default)]
    pub private: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CategoriesFile {
    pub macros: HashMap<String, Filter>,
    pub categories: Vec<CategoryDef>,
    /// Priority-ordered evaluation list compiled from the `excludes` relation (see `build_order`).
    /// First-match over this reproduces the exclude semantics *without* evaluating excludes —
    /// each node's condition is tried in order; the first match wins. Not part of the JSON.
    #[serde(skip)]
    pub order: Vec<OrderedNode>,
    /// Discrimination net over `order`, compiled by `build_order` after the priority order is
    /// known. Prunes `categorize`'s first-match walk to a small candidate set; identical results.
    #[serde(skip)]
    pub tree: DecisionTree,
}

/// One entry in the compiled priority order: try `condition`; on match, either select the
/// category or (for a disqualifier-macro sink) skip the object.
#[derive(Debug, Clone)]
pub enum OrderedNode {
    /// A real category — index into `CategoriesFile::categories`.
    Category { idx: usize },
    /// A disqualifier macro (e.g. `data_no`): matching means "no category" for this object.
    Skip { condition: Filter },
}

/// Find the first matching category via the compiled priority order (`build_order`). Pure
/// first-match: the first node whose condition matches wins — a `Category` node is the answer,
/// a `Skip` (disqualifier-macro) node means the object has no category. No `excludes` are
/// evaluated at runtime; the ordering already encodes them (see `build_order`).
pub fn categorize<'a>(ctx: &ExtractCtx, cats: &'a CategoriesFile) -> Option<&'a CategoryDef> {
    // The discrimination net prunes the priority list to a small, order-preserving candidate set;
    // first-match `eval` over it is identical to the full walk (the tree only drops provably-false
    // nodes — see `decision_tree`).
    for &i in cats.tree.candidates(ctx) {
        if let Some(hit) = eval_node(&cats.order[i], ctx, cats) {
            return hit;
        }
    }
    None
}

/// Full first-match walk over the whole `order`, bypassing the tree. Kept as the reference
/// implementation for the differential test (`categorize` must agree with it for every object).
pub fn categorize_linear<'a>(ctx: &ExtractCtx, cats: &'a CategoriesFile) -> Option<&'a CategoryDef> {
    for node in &cats.order {
        if let Some(hit) = eval_node(node, ctx, cats) {
            return hit;
        }
    }
    None
}

/// Evaluate one order node against `ctx`. `Some(Some(cat))` = category matched (answer);
/// `Some(None)` = disqualifier matched (no category); `None` = no match, continue. Both
/// `cat.condition` and a `Skip`'s `condition` are already fully macro/sanitizer-resolved by load
/// time (see `TopicRunner::load`), so `eval` needs nothing beyond `ctx`.
fn eval_node<'a>(
    node: &OrderedNode,
    ctx: &ExtractCtx,
    cats: &'a CategoriesFile,
) -> Option<Option<&'a CategoryDef>> {
    match node {
        OrderedNode::Category { idx } => {
            let cat = &cats.categories[*idx];
            eval(&cat.condition, ctx).then_some(Some(cat))
        }
        OrderedNode::Skip { condition } => eval(condition, ctx).then_some(None),
    }
}
