//! JSON node/edge tree for a `Producer` (and the `Sanitizer` chains hanging off its `Extract`
//! leaves) — the same walk `bin/plot_dag` does to emit Graphviz DOT, but structured for a browser
//! graph renderer instead. Kept as a lib module (not folded into `bin/plot_dag`) so both the DOT
//! binary and `bin/dag_json` (the live editor's backend) can build a tree from the same `Producer`
//! without duplicating the walk.

use serde::Serialize;

use serde_json::{Map, Value};

use crate::categorize::categories::{CategoriesFile, OrderedNode};
use crate::lang::extract::Extract;
use crate::lang::filter::Filter;
use crate::lang::producer::{MatchOrigin, Producer};
use crate::lang::sanitize::{Sanitizer, Step};

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
    render_producer(&mut g, producer, None);
    g
}

/// Renders `p`, returning its own node id (for the caller to wire a value-flow edge to). `annotate_to`
/// is where *this producer's own* `annotate` (not any nested producer's) should attach — the nearest
/// condition/rule ancestor, i.e. the caller's own anchor, not the producer's own freshly-made node:
/// an annotate describes why a *branch* produced what it did, and the branch is identified by its
/// condition, not by however many hops (a `Parent` wrap, a further nested `Match`) the value takes to
/// actually resolve. `None` only at the tree root, where there is no condition to attach to yet, so
/// it falls back to the producer's own node.
fn render_producer(g: &mut DagGraph, p: &Producer, annotate_to: Option<&str>) -> String {
    match p {
        Producer::Match { rules: _, default, annotate, origin: MatchOrigin::Default } => {
            // No real branching — a `defaults` JSON entry bundled straight into a producer
            // (`topic::runner::default_value_producer`), always empty `rules`. Shown as a plain
            // literal, not a one-branch "match" wrapper around nothing.
            let d = default.as_ref().expect("MatchOrigin::Default always carries a default");
            let label = format!("default\nvalue: {}", truncate(&format!("{d:?}"), 40));
            let node = g.node(label, "const");
            annotate_node(g, annotate_to.unwrap_or(&node), annotate);
            node
        }
        Producer::Match { rules, default, annotate, origin } => {
            // `Fallback`/`TagOr` matches always have `when: true` on every rule by construction
            // (see `MatchOrigin`'s own doc) — describing a condition that's always "true" is noise,
            // so those show priority order instead of the (uninformative) condition text.
            let priority_only = matches!(origin, MatchOrigin::Fallback | MatchOrigin::TagOr);
            let label = match origin {
                MatchOrigin::Rules => format!("match\n{} rule(s)", rules.len()),
                MatchOrigin::Fallback => format!("fallback\n{} branch(es)", rules.len()),
                MatchOrigin::ParentOrObj => "parent_or_obj".to_owned(),
                MatchOrigin::TagOr => "tag_or".to_owned(),
                MatchOrigin::Default => unreachable!("handled above"),
            };
            let node = g.node(label, "match");
            annotate_node(g, annotate_to.unwrap_or(&node), annotate);
            // Each rule is its own branch, and its value producer (which may itself be a further
            // `Match`) hangs off that node so the tree actually branches instead of cramming every
            // rule into one node's text. The rule node is the value's `annotate_to` — its own
            // condition, not the value node however many hops down it ends up.
            for (i, r) in rules.iter().enumerate() {
                let rule_label = if priority_only { format!("priority {}", i + 1) } else { truncate(&r.when.describe(), 120) };
                let rule_node = g.node(rule_label, "rule");
                g.edge(&node, &rule_node, "");
                let value_node = render_producer(g, &r.value, Some(&rule_node));
                g.edge(&rule_node, &value_node, "");
            }
            if let Some(d) = default {
                let default_node = g.node(format!("const\nvalue: {}", truncate(&format!("{d:?}"), 40)), "const");
                g.edge(&node, &default_node, "default");
            }
            node
        }
        Producer::Extract { extract, annotate } => {
            let (label, kind) = match extract {
                Extract::Value { key, .. } => (format!("extract\nkey: {key}"), "extract"),
                Extract::Candidates { keys, .. } => (format!("extract\nkeys: {keys:?}"), "extract"),
            };
            let node = g.node(label, kind);
            annotate_node(g, annotate_to.unwrap_or(&node), annotate);
            if let Some(chain) = extract.sanitize() {
                let chain_root = render_chain(g, chain);
                g.edge(&node, &chain_root, "sanitize");
            }
            node
        }
        Producer::Const { value, annotate } => {
            let label = format!("const\nvalue: {}", truncate(&format!("{value:?}"), 40));
            let node = g.node(label, "const");
            annotate_node(g, annotate_to.unwrap_or(&node), annotate);
            node
        }
        Producer::Parent(inner) => {
            // Not a condition itself — transparent plumbing (a tagset switch), so it forwards
            // whatever `annotate_to` it received rather than substituting its own node: `inner`'s
            // annotate still belongs to the nearest real condition, same as if `Parent` weren't there.
            let node = g.node("parent".to_owned(), "parent");
            let child = render_producer(g, inner, annotate_to);
            g.edge(&node, &child, "");
            node
        }
    }
}

fn render_chain(g: &mut DagGraph, chain: &Sanitizer) -> String {
    let steps = chain.steps();
    let mut prev: Option<String> = None;
    let mut first: Option<String> = None;
    for step in steps {
        let id = g.node(step_label(step), "step");
        if first.is_none() {
            first = Some(id.clone());
        }
        if let Some(p) = &prev {
            g.edge(p, &id, "");
        }
        prev = Some(id);
    }
    first.unwrap_or_else(|| g.node("(empty chain)".to_owned(), "step"))
}

fn step_label(step: &Step) -> String {
    match step {
        Step::Mapping { mapping, on_miss } => {
            format!("mapping\n{} entries\non_miss: {:?}", mapping.len(), on_miss.as_deref().unwrap_or("drop"))
        }
        Step::Replace { replace } => format!("replace\n{} rule(s)", replace.len()),
        Step::Builtin(name) => format!("builtin: {name}"),
    }
}

/// The categorization tree for one `ElementKind`'s `CategoriesFile` — which category (or no
/// category, for a disqualifier) an object gets, tried in `order` (the compiled priority list;
/// see `categories::build_order`), each branch's condition rendered as a `Filter` expression tree.
/// A flat, single-level branch per `order` entry (not a discrimination net like `DecisionTree`) —
/// it mirrors `categorize_linear`'s reference walk, which is what a human actually reads a
/// category set as: try these conditions in order, first match wins.
pub fn category_order_dag(cats: &CategoriesFile) -> DagGraph {
    let mut g = DagGraph::default();
    let root = g.node(format!("categorize\n{} rule(s)", cats.order.len()), "match");
    for (i, node) in cats.order.iter().enumerate() {
        match node {
            OrderedNode::Category { idx } => {
                let cat = &cats.categories[*idx];
                let branch = g.node(format!("priority {}\ncategory: {}", i + 1, cat.id), "rule");
                g.edge(&root, &branch, "");
                let cond = render_filter(&mut g, &cat.condition);
                g.edge(&branch, &cond, "if");
            }
            OrderedNode::Skip { condition } => {
                let branch = g.node(format!("priority {}\n(no category)", i + 1), "sanitizer");
                g.edge(&root, &branch, "");
                let cond = render_filter(&mut g, condition);
                g.edge(&branch, &cond, "if");
            }
        }
    }
    g
}

/// Renders `f`, returning its own node id. Combinators (`and`/`or`/`not`) get their own branching
/// node so nested boolean structure is visible in the graph; every other `Filter` variant already
/// has a precise one-line rendering (`Filter::describe`), so it's a single leaf node rather than
/// broken down further (there's nothing more to a tag predicate that a sub-tree would clarify).
fn render_filter(g: &mut DagGraph, f: &Filter) -> String {
    match f {
        Filter::And { and } => {
            let node = g.node(format!("and\n{} clause(s)", and.len()), "match");
            for c in and {
                let child = render_filter(g, c);
                g.edge(&node, &child, "");
            }
            node
        }
        Filter::Or { or } => {
            let node = g.node(format!("or\n{} clause(s)", or.len()), "match");
            for c in or {
                let child = render_filter(g, c);
                g.edge(&node, &child, "");
            }
            node
        }
        Filter::Not { not } => {
            let node = g.node("not".to_owned(), "parent");
            let child = render_filter(g, not);
            g.edge(&node, &child, "");
            node
        }
        other => g.node(truncate(&other.describe(), 120), "extract"),
    }
}
