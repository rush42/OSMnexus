//! JSON node/edge tree for a `Producer` (and the `Sanitizer` chains hanging off its `Extract`
//! leaves) — the same walk `bin/plot_dag` does to emit Graphviz DOT, but structured for a browser
//! graph renderer instead. Kept as a lib module (not folded into `bin/plot_dag`) so both the DOT
//! binary and `bin/dag_json` (the live editor's backend) can build a tree from the same `Producer`
//! without duplicating the walk.

use std::fmt::Write as _;

use serde::Serialize;

use crate::tag_engine::extract::Extract;
use crate::tag_engine::producer::Producer;
use crate::tag_engine::sanitize::{SanitizeRef, Sanitizer, Step};

#[derive(Serialize)]
pub struct DagNode {
    pub id: String,
    pub label: String,
    /// One of "root"/"match"/"extract"/"directed_extract"/"const"/"parent"/"sanitizer"/"step" —
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

/// Build a labeled root node (`label`, e.g. `"<topic>\n<field>\n[<categories>]"`) wired to `producer`'s tree.
pub fn root_dag(root_label: &str, producer: &Producer) -> DagGraph {
    let mut g = DagGraph::default();
    let root = g.node(root_label.to_owned(), "root");
    let child = render_producer(&mut g, producer);
    g.edge(&root, &child, "");
    g
}

fn render_producer(g: &mut DagGraph, p: &Producer) -> String {
    match p {
        Producer::Match { rules, default, annotate } => {
            let mut label = format!("match\n{} rule(s)", rules.len());
            for r in rules.iter().take(6) {
                let _ = write!(label, "\n  {} => {}", truncate(&format!("{:?}", r.when), 40), truncate(&format!("{:?}", r.value), 20));
            }
            if rules.len() > 6 {
                let _ = write!(label, "\n  … +{} more", rules.len() - 6);
            }
            if let Some(d) = default {
                let _ = write!(label, "\ndefault => {}", truncate(&format!("{d:?}"), 20));
            }
            if !annotate.is_empty() {
                let _ = write!(label, "\nannotate: {}", truncate(&format!("{annotate:?}"), 40));
            }
            g.node(label, "match")
        }
        Producer::Extract { extract, sanitize, annotate } => {
            let mut label = String::from("extract");
            match extract {
                Extract::Value { key } => { let _ = write!(label, "\nkey: {key}"); }
                Extract::Candidates { keys } => { let _ = write!(label, "\nkeys: {keys:?}"); }
            }
            if !annotate.is_empty() {
                let _ = write!(label, "\nannotate: {}", truncate(&format!("{annotate:?}"), 40));
            }
            let node = g.node(label, "extract");
            if let Some(sref) = sanitize {
                let chain_root = render_sanitize_ref(g, sref);
                g.edge(&node, &chain_root, "sanitize");
            }
            node
        }
        Producer::DirectedExtract { key, from, sanitize, annotate } => {
            let mut label = format!("directed extract\nkey: {key}\nfrom: {from:?}");
            if !annotate.is_empty() {
                let _ = write!(label, "\nannotate: {}", truncate(&format!("{annotate:?}"), 40));
            }
            let node = g.node(label, "directed_extract");
            if let Some(sref) = sanitize {
                let chain_root = render_sanitize_ref(g, sref);
                g.edge(&node, &chain_root, "sanitize");
            }
            node
        }
        Producer::Const { value, annotate } => {
            let mut label = format!("const\nvalue: {}", truncate(&format!("{value:?}"), 40));
            if !annotate.is_empty() {
                let _ = write!(label, "\nannotate: {}", truncate(&format!("{annotate:?}"), 40));
            }
            g.node(label, "const")
        }
        Producer::Parent(inner) => {
            let node = g.node("parent".to_owned(), "parent");
            let child = render_producer(g, inner);
            g.edge(&node, &child, "");
            node
        }
    }
}

fn render_sanitize_ref(g: &mut DagGraph, sref: &SanitizeRef) -> String {
    match sref {
        SanitizeRef::Name(name) => g.node(format!("sanitizer: {name}\n(unresolved)"), "sanitizer"),
        SanitizeRef::Inline(chain) => render_chain(g, chain),
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
