//! JSON node/edge tree for a `Producer` (and the `Sanitizer` chains hanging off its `Extract`
//! leaves) — the same walk `bin/plot_dag` does to emit Graphviz DOT, but structured for a browser
//! graph renderer instead. Kept as a lib module (not folded into `bin/plot_dag`) so both the DOT
//! binary and `bin/dag_json` (the live editor's backend) can build a tree from the same `Producer`
//! without duplicating the walk.

use serde::Serialize;

use serde_json::{Map, Value};

use crate::categorize::categories::{CategoriesFile, OrderedNode};
use crate::decision_tree::DecisionTree;
use crate::lang::extract::Extract;
use crate::lang::filter::Filter;
use crate::lang::producer::{MatchOrigin, Producer};
use crate::lang::sanitize::{ReplaceAt, Sanitizer};

#[derive(Serialize)]
pub struct DagNode {
    pub id: String,
    pub label: String,
    /// One of "match"/"rule"/"extract"/"const"/"parent"/"sanitizer"/"step" —
    /// lets the frontend style nodes by kind without parsing `label`.
    pub kind: &'static str,
}

#[derive(Serialize)]
pub struct DagEdge {
    pub id: String,
    pub source: String,
    pub target: String,
    pub label: String,
}

#[derive(Serialize, Default)]
pub struct DagGraph {
    pub nodes: Vec<DagNode>,
    pub edges: Vec<DagEdge>,
}

impl DagGraph {
    fn node(&mut self, label: String, kind: &'static str) -> String {
        let id = format!("n{}", self.nodes.len());
        self.nodes.push(DagNode { id: id.clone(), label, kind });
        id
    }

    fn edge(&mut self, source: &str, target: &str, label: &str) {
        let id = format!("e{}", self.edges.len());
        self.edges.push(DagEdge { id, source: source.to_owned(), target: target.to_owned(), label: label.to_owned() });
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max { s.to_owned() } else { format!("{}…", s.chars().take(max).collect::<String>()) }
}

/// If `annotate` is non-empty, give it its own node off `owner` (kind "annotate", edge labeled
/// "annotate") instead of cramming it into `owner`'s own label — the frontend positions an
/// "annotate" node beside its owner rather than as a real tree child (see `DagView.tsx`'s
/// `layoutTree`), so it reads as a side note on the branch, not another step in the value's flow.
fn annotate_node(g: &mut DagGraph, owner: &str, annotate: &Map<String, Value>) {
    if annotate.is_empty() {
        return;
    }
    let label = annotate.iter().map(|(k, v)| format!("{k}: {v}")).collect::<Vec<_>>().join("\n");
    let node = g.node(label, "annotate");
    g.edge(owner, &node, "annotate");
}

/// The pure `Producer` tree — no synthetic root node, no "who uses this" bookkeeping (that's a
/// `topic::runner`/`bin/dag_json` concern; a node here is only ever a step in how the value itself
/// gets built: `Match`/`Parent` branches down to `Extract`/`Const` leaves, plus
/// any `Sanitizer` chain hanging off an `Extract`).
pub fn producer_dag(producer: &Producer) -> DagGraph {
    let mut g = DagGraph::default();
    render_producer(&mut g, producer);
    g
}

fn render_producer(g: &mut DagGraph, p: &Producer) -> String {
    match p {
        Producer::Match { rules: _, default, annotate, origin: MatchOrigin::Default, tree: _ } => {
            // No real branching — a `defaults` JSON entry bundled straight into a producer
            // (`topic::runner::default_value_producer`), always empty `rules`. Shown as a plain
            // literal, not a one-branch "match" wrapper around nothing.
            let d = default.as_ref().expect("MatchOrigin::Default always carries a default");
            let label = format!("Default\n{}", truncate(&d.to_string(), 40));
            let node = g.node(label, "const");
            annotate_node(g, &node, annotate);
            node
        }
        Producer::Match { rules, default, annotate, origin, tree: _ } => {
            // `Fallback`/`TagOr` matches always have `when: true` on every rule by construction
            // (see `MatchOrigin`'s own doc) — describing a condition that's always "true" is noise,
            // so those show priority order instead of the (uninformative) condition text.
            let priority_only = matches!(origin, MatchOrigin::Fallback | MatchOrigin::TagOr);
            let label = match origin {
                MatchOrigin::Rules => "Match".to_owned(),
                MatchOrigin::Fallback => format!("Fallback\n{} branch(es)", rules.len()),
                MatchOrigin::ParentOrObj => "Parent Or Obj".to_owned(),
                MatchOrigin::TagOr => "Tag Or".to_owned(),
                MatchOrigin::Default => unreachable!("handled above"),
            };
            let node = g.node(label, "match");
            annotate_node(g, &node, annotate);
            // Each rule is its own branch, and its value producer (which may itself be a further
            // `Match`) hangs off that node so the tree actually branches instead of cramming every
            // rule into one node's text.
            for (i, r) in rules.iter().enumerate() {
                let rule_label = if matches!(origin, MatchOrigin::Fallback) {
                    format!("Option {}", i + 1)
                } else if priority_only {
                    format!("{}", i + 1)
                } else {
                    truncate(&r.when.describe(), 120)
                };
                let rule_node = g.node(rule_label, "rule");
                g.edge(&node, &rule_node, "");
                let value_node = render_producer(g, &r.value);
                g.edge(&rule_node, &value_node, "");
            }
            if let Some(d) = default {
                let default_node = g.node(format!("Const\n{}", truncate(&d.to_string(), 40)), "const");
                g.edge(&node, &default_node, "default");
            }
            node
        }
        Producer::Extract { extract, annotate } => {
            let (label, kind) = match extract {
                Extract::Value { key, .. } => (format!("Extract\nkey: {key}"), "extract"),
                Extract::Candidates { keys, .. } => (format!("Extract\nkeys: {keys:?}"), "extract"),
            };
            let node = g.node(label, kind);
            annotate_node(g, &node, annotate);
            let chain = extract.sanitize();
            if chain.is_empty() { node } else { render_chain(g, chain, &node) }
        }
        Producer::Const { value, annotate } => {
            let label = format!("Const\n{}", truncate(&value.to_string(), 40));
            let node = g.node(label, "const");
            annotate_node(g, &node, annotate);
            node
        }
        Producer::Parent(inner) => {
            let node = g.node("Parent".to_owned(), "parent");
            let child = render_producer(g, inner);
            g.edge(&node, &child, "");
            node
        }
    }
}

/// Renders `chain`'s steps as a line feeding into `sink` (the `Extract` leaf they sanitize), each
/// step wired to the next in application order and the last wired to `sink` — so the tree reads as
/// the actual data flow (sanitize steps, then the extract they feed) rather than a side branch
/// hanging off the extract. Returns the entry point: the first step's node, or `sink` itself if the
/// chain has no steps.
fn render_chain(g: &mut DagGraph, chain: &[Sanitizer], sink: &str) -> String {
    let mut next = sink.to_owned();
    for step in chain.iter().rev() {
        let id = g.node(step_label(step), "step");
        g.edge(&id, &next, "");
        next = id;
    }
    next
}

/// A named sanitizer's own chain, standalone — independent of any `Extract` leaf that happens to
/// reference it (unlike `render_chain`, which only ever appears as a tail hanging off one). Reads
/// top-down: the root names the sanitizer, each step feeds the next in application order, ending
/// at the last step (there's no downstream consumer to wire into here — this *is* the sink).
pub fn sanitizer_dag(name: &str, chain: &[Sanitizer]) -> DagGraph {
    let mut g = DagGraph::default();
    let root = g.node(format!("Sanitizer\n{name}"), "sanitizer");
    if chain.is_empty() {
        let identity = g.node("Identity\n(no steps)".to_owned(), "const");
        g.edge(&root, &identity, "");
        return g;
    }
    let mut prev = root;
    for step in chain {
        let node = g.node(step_label(step), "step");
        g.edge(&prev, &node, "");
        prev = node;
    }
    g
}

fn step_label(step: &Sanitizer) -> String {
    match step {
        Sanitizer::Mapping { mapping, on_miss } => {
            let entries = mapping.iter().map(|(k, v)| match v {
                Some(v) => format!("{k} -> {}", v.clone().into_value()),
                None => format!("{k} -> (drop)"),
            });
            let lines: Vec<String> = std::iter::once("Mapping".to_owned())
                .chain(entries)
                .chain(std::iter::once(format!("on_miss: {}", on_miss.as_deref().unwrap_or("drop"))))
                .collect();
            lines.join("\n")
        }
        Sanitizer::Replace { replace } => {
            let lines: Vec<String> = std::iter::once("Replace".to_owned())
                .chain(replace.iter().map(|r| {
                    if r.at == ReplaceAt::Prefix { format!("{} -> {} (prefix)", r.from, r.to) } else { format!("{} -> {}", r.from, r.to) }
                }))
                .collect();
            lines.join("\n")
        }
        Sanitizer::Builtin(name) => format!("Builtin\nname: {name}"),
    }
}

/// One `order` entry's (see `categories::build_order`) own condition, rendered as a `Filter`
/// expression tree under a header node naming it — and, for a real category, what it excludes.
/// The categorize view used to cram every category into one "try these in order" tree at once
/// (`categorize_linear`'s reference walk, laid out flat); that stopped being readable past a
/// handful of categories, so the live editor now drives this one-category-at-a-time view off a
/// dropdown instead, ordered the same way `order` (the compiled priority list) already is — see
/// `bin/dag_json.rs`'s category-mode list/selector handling.
pub fn category_condition_dag(cats: &CategoriesFile, order_idx: usize) -> DagGraph {
    let mut g = DagGraph::default();
    let node = &cats.order[order_idx];
    let (header_label, condition) = match node {
        OrderedNode::Category { idx } => {
            let cat = &cats.categories[*idx];
            let mut label = format!("Category\n{}", cat.id);
            if let Some(excludes) = cat.excludes.as_ref().filter(|e| !e.is_empty()) {
                label.push_str(&format!("\nexcludes: {}", excludes.join(", ")));
            }
            (label, &cat.condition)
        }
        OrderedNode::Skip { condition } => ("No Category".to_owned(), condition),
    };
    let header = g.node(header_label, "rule");
    let cond_node = render_filter(&mut g, condition);
    g.edge(&header, &cond_node, "if");
    g
}

/// Renders `f`, returning its own node id. Combinators (`and`/`or`/`not`) get their own branching
/// node so nested boolean structure is visible in the graph; every other `Filter` variant already
/// has a precise one-line rendering (`Filter::describe`), so it's a single leaf node rather than
/// broken down further (there's nothing more to a tag predicate that a sub-tree would clarify).
fn render_filter(g: &mut DagGraph, f: &Filter) -> String {
    match f {
        Filter::And { and } => {
            let node = g.node("And".to_owned(), "match");
            for c in and {
                let child = render_filter(g, c);
                g.edge(&node, &child, "");
            }
            node
        }
        Filter::Or { or } => {
            let node = g.node("Or".to_owned(), "match");
            for c in or {
                let child = render_filter(g, c);
                g.edge(&node, &child, "");
            }
            node
        }
        Filter::Not { not } => {
            let node = g.node("Not".to_owned(), "parent");
            let child = render_filter(g, not);
            g.edge(&node, &child, "");
            node
        }
        other => g.node(truncate(&other.describe(), 120), "extract"),
    }
}

/// The compiled discrimination net (`decision_tree::DecisionTree`) that prunes
/// `categorize`'s first-match walk for one `ElementKind` — unlike `category_order_dag`'s flat
/// priority list, this shows the actual branch-on-tag/atom structure the tree walks per object,
/// with each leaf naming the (small, order-preserving) subset of categories/skips it still has to
/// try. Node/edge shape mirrors `producer_dag`: reuses the "match"/"rule" kinds for branches so the
/// frontend styles them consistently, with leaves rendered as their own kind.
pub fn decision_tree_dag(cats: &CategoriesFile) -> DagGraph {
    let mut g = DagGraph::default();
    render_decision_tree(&mut g, &cats.tree, cats);
    g
}

pub fn order_label(cats: &CategoriesFile, idx: usize) -> String {
    match &cats.order[idx] {
        OrderedNode::Category { idx } => cats.categories[*idx].id.clone(),
        OrderedNode::Skip { .. } => "(no category)".to_owned(),
    }
}

fn render_decision_tree(g: &mut DagGraph, tree: &DecisionTree, cats: &CategoriesFile) -> String {
    match tree {
        DecisionTree::Leaf(idxs) => {
            let label = if idxs.is_empty() {
                "Leaf\n(no candidates)".to_owned()
            } else {
                let mut lines = vec!["Leaf".to_owned()];
                lines.extend(idxs.iter().map(|(i, _)| order_label(cats, *i)));
                truncate(&lines.join("\n"), 200)
            };
            g.node(label, "const")
        }
        DecisionTree::Branch { tag, children, wildcard } => {
            let node = g.node(format!("branch\ntag: {tag}"), "match");
            let mut keys: Vec<&String> = children.keys().collect();
            keys.sort();
            for k in keys {
                let child = render_decision_tree(g, &children[k], cats);
                g.edge(&node, &child, k);
            }
            let wild = render_decision_tree(g, wildcard, cats);
            g.edge(&node, &wild, "*");
            node
        }
        DecisionTree::AtomBranch { atom, on_true, on_false } => {
            let node = g.node(format!("branch\natom: {}", truncate(&format!("{atom:?}"), 80)), "match");
            let t = render_decision_tree(g, on_true, cats);
            g.edge(&node, &t, "true");
            let f = render_decision_tree(g, on_false, cats);
            g.edge(&node, &f, "false");
            node
        }
    }
}
