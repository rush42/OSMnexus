//! Graphviz DOT backend: a topic output field's `Producer` tree, with each `Extract` leaf's
//! resolved `sanitize` chain drawn as a subgraph hanging off it. Moved here from `bin/plot_tree`,
//! which now only decides which fields/variants to plot and writes the files.

use std::fmt::Write as _;

use osmnexus_lang::lang::extract::Extract;
use osmnexus_lang::lang::producer::Producer;
use osmnexus_lang::lang::sanitize::Sanitizer;

/// A small id/label/shape allocator so `render_producer`/`render_sanitizer` just push edges.
struct Graph {
    lines: Vec<String>,
    next_id: usize,
}

impl Graph {
    fn node(&mut self, label: &str, shape: &str, color: &str) -> String {
        let id = format!("n{}", self.next_id);
        self.next_id += 1;
        let escaped = escape(label);
        self.lines.push(format!(
            "  {id} [label=\"{escaped}\", shape={shape}, style=filled, fillcolor=\"{color}\"];\n"
        ));
        id
    }

    fn edge(&mut self, from: &str, to: &str, label: &str) {
        if label.is_empty() {
            self.lines.push(format!("  {from} -> {to};\n"));
        } else {
            self.lines.push(format!("  {from} -> {to} [label=\"{}\"];\n", escape(label)));
        }
    }
}

fn escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n")
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max { s.to_owned() } else { format!("{}…", s.chars().take(max).collect::<String>()) }
}

/// Render one output field's `Producer` tree (and the sanitizer chains under its `Extract`
/// leaves) as a Graphviz DOT document. `labels` names who produces the field this way (the topic
/// default and/or categories).
pub fn render_dot(topic: &str, field: &str, labels: &[String], producer: &Producer) -> String {
    let mut g = Graph { lines: Vec::new(), next_id: 0 };
    let label_list = if labels.len() > 6 {
        format!("{}, … +{} more", labels[..6].join(", "), labels.len() - 6)
    } else {
        labels.join(", ")
    };
    let root = g.node(&format!("{topic}\n{field}\n[{label_list}]"), "box", "#dbe9f6");
    let child = render_producer(&mut g, producer);
    g.edge(&root, &child, "");
    format!(
        "digraph tree {{\n  rankdir=TB;\n  node [fontname=\"sans-serif\", fontsize=11];\n  edge [fontname=\"sans-serif\", fontsize=9];\n{}}}\n",
        g.lines.join("")
    )
}

fn render_producer(g: &mut Graph, p: &Producer) -> String {
    match p {
        Producer::Match { rules, default, annotate, .. } => {
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
            g.node(&label, "note", "#e2f0d9")
        }
        Producer::Extract { extract, annotate } => {
            let mut label = match extract {
                Extract::Candidates { alts, .. } => {
                    format!("extract\nkeys: {:?}", alts.iter().map(Extract::label).collect::<Vec<_>>())
                }
                _ => format!("extract\nkey: {}", extract.label()),
            };
            if !annotate.is_empty() {
                let _ = write!(label, "\nannotate: {}", truncate(&format!("{annotate:?}"), 40));
            }
            let node = g.node(&label, "box", "#d9e8fb");
            let chain = extract.sanitize();
            if !chain.is_empty() {
                let chain_root = render_chain(g, chain);
                g.edge(&node, &chain_root, "sanitize");
            }
            node
        }
        Producer::Const { value, annotate } => {
            let mut label = format!("const\nvalue: {}", truncate(&format!("{value:?}"), 40));
            if !annotate.is_empty() {
                let _ = write!(label, "\nannotate: {}", truncate(&format!("{annotate:?}"), 40));
            }
            g.node(&label, "box", "#d9e8fb")
        }
        Producer::Parent(inner) => {
            let node = g.node("parent", "diamond", "#fff2cc");
            let child = render_producer(g, inner);
            g.edge(&node, &child, "");
            node
        }
    }
}

fn render_chain(g: &mut Graph, chain: &[Sanitizer]) -> String {
    let mut prev: Option<String> = None;
    let mut first: Option<String> = None;
    for step in chain {
        let id = g.node(&step_label(step), "ellipse", "#ead1dc");
        if first.is_none() {
            first = Some(id.clone());
        }
        if let Some(p) = &prev {
            g.edge(p, &id, "");
        }
        prev = Some(id);
    }
    first.unwrap_or_else(|| g.node("(empty chain)", "ellipse", "#ead1dc"))
}

fn step_label(step: &Sanitizer) -> String {
    match step {
        Sanitizer::Mapping { mapping, on_miss } => {
            format!("mapping\n{} entries\non_miss: {:?}", mapping.len(), on_miss.as_deref().unwrap_or("drop"))
        }
        Sanitizer::Replace { replace } => format!("replace\n{} rule(s)", replace.len()),
        Sanitizer::Builtin(b) => format!("builtin: {}", b.name()),
    }
}
