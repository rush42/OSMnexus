use std::collections::HashMap;

use serde::Deserialize;

use crate::engine::topic::DeriverBinding;

// Re-export the predicate/minzoom types + loaders so existing `classify::categories::*` import
// paths keep working after the split into `filter` and `loader`.
pub use crate::classify::filter::{
    eval_filter, resolve_minzoom, CategoryContext, Filter, MinzoomCase, MinzoomRule,
};
pub use crate::classify::loader::{load_categories_dir, load_topic_categories, load_shared_macros};

use crate::classify::decision_tree::{self, DecisionTree};
use crate::classify::filter::eval;

// ── Data types ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
pub struct CategoryDef {
    pub id: String,
    pub condition: Filter,
    pub excludes: Option<Vec<String>>,
    /// Per-category minzoom override. Falls back to the topic-level default when absent.
    #[serde(default)]
    pub minzoom: Option<MinzoomRule>,
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
    /// Scope of parking-based `traffic_mode` inference for this category: `"both"` (infer both
    /// sides, e.g. bicycle roads) or `"directional"` (infer only the transformed side, e.g.
    /// on-highway cycleway lanes). Absent = no parking inference.
    #[serde(default)]
    pub parking_inference: Option<String>,
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

impl CategoriesFile {
    /// Compile `categories` + their disqualifier-macro excludes into a single priority-ordered
    /// evaluation list, so `categorize` is pure first-match with no runtime `excludes` checks.
    ///
    /// `X excludes Y` means Y beats X, so Y must precede X (edge `Y → X`). Topo-sort the graph
    /// (categories ∪ macro sinks); a cycle is a contradictory priority and is a hard error.
    /// Correctness relies on the disjointness invariant (`categories_are_disjoint`): any two nodes
    /// that can co-match have an exclude edge, so first-match-in-order picks the same winner.
    pub fn build_order(&mut self, tree_max_depth: usize) -> anyhow::Result<()> {
        use std::collections::{BTreeMap, BTreeSet};

        let catset: BTreeSet<&str> = self.categories.iter().map(|c| c.id.as_str()).collect();
        let mut nodes: BTreeSet<String> = catset.iter().map(|s| s.to_string()).collect();
        let mut succ: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        let mut indeg: BTreeMap<String, usize> = nodes.iter().map(|n| (n.clone(), 0)).collect();

        for c in &self.categories {
            for y in c.excludes.iter().flatten() {
                nodes.insert(y.clone());
                indeg.entry(y.clone()).or_insert(0);
                // Edge y -> c.id (y precedes the category that excludes it).
                if succ.entry(y.clone()).or_default().insert(c.id.clone()) {
                    *indeg.get_mut(&c.id).expect("category has indeg entry") += 1;
                }
            }
        }

        // Kahn's algorithm; a BTreeSet as the ready-queue gives a deterministic alphabetical
        // tiebreak (order among nodes without an edge is irrelevant — they're disjoint).
        let mut ready: BTreeSet<String> =
            nodes.iter().filter(|n| indeg[*n] == 0).cloned().collect();
        let mut order_names: Vec<String> = Vec::with_capacity(nodes.len());
        while let Some(n) = ready.iter().next().cloned() {
            ready.remove(&n);
            if let Some(ss) = succ.get(&n) {
                for m in ss {
                    let d = indeg.get_mut(m).expect("successor has indeg entry");
                    *d -= 1;
                    if *d == 0 {
                        ready.insert(m.clone());
                    }
                }
            }
            order_names.push(n);
        }

        anyhow::ensure!(
            order_names.len() == nodes.len(),
            "cyclic `excludes` relation: cannot build a priority order (involved: {:?})",
            nodes.iter().filter(|n| !order_names.contains(n)).collect::<Vec<_>>(),
        );

        let idx_of: BTreeMap<&str, usize> =
            self.categories.iter().enumerate().map(|(i, c)| (c.id.as_str(), i)).collect();
        let mut order = Vec::with_capacity(order_names.len());
        for name in &order_names {
            match idx_of.get(name.as_str()) {
                Some(&idx) => order.push(OrderedNode::Category { idx }),
                None => {
                    let condition = self.macros.get(name).cloned().ok_or_else(|| {
                        anyhow::anyhow!("exclude references unknown category/macro '{name}'")
                    })?;
                    order.push(OrderedNode::Skip { condition });
                }
            }
        }
        self.order = order;
        self.tree = decision_tree::build(self, tree_max_depth);
        Ok(())
    }
}

/// Find the first matching category via the compiled priority order (`build_order`). Pure
/// first-match: the first node whose condition matches wins — a `Category` node is the answer,
/// a `Skip` (disqualifier-macro) node means the object has no category. No `excludes` are
/// evaluated at runtime; the ordering already encodes them (see `build_order`).
pub fn categorize<'a>(ctx: &CategoryContext, cats: &'a CategoriesFile) -> Option<&'a CategoryDef> {
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
pub fn categorize_linear<'a>(ctx: &CategoryContext, cats: &'a CategoriesFile) -> Option<&'a CategoryDef> {
    for node in &cats.order {
        if let Some(hit) = eval_node(node, ctx, cats) {
            return hit;
        }
    }
    None
}

/// Evaluate one order node against `ctx`. `Some(Some(cat))` = category matched (answer);
/// `Some(None)` = disqualifier matched (no category); `None` = no match, continue.
fn eval_node<'a>(
    node: &OrderedNode,
    ctx: &CategoryContext,
    cats: &'a CategoriesFile,
) -> Option<Option<&'a CategoryDef>> {
    match node {
        OrderedNode::Category { idx } => {
            let cat = &cats.categories[*idx];
            eval(&cat.condition, ctx, &cats.macros).then_some(Some(cat))
        }
        OrderedNode::Skip { condition } => eval(condition, ctx, &cats.macros).then_some(None),
    }
}

