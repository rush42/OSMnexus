//! Load-time compilation of a `CategoriesFile`'s `excludes` relation into a priority-ordered
//! evaluation list plus its discrimination-net pruning index.

use crate::tag_engine::loader::decision_tree;
use crate::tag_engine::producer::categories::{CategoriesFile, OrderedNode};

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
