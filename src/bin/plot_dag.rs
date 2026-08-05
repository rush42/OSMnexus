//! Plot a topic's output `Producer` trees (and the sanitizer chains hanging off their `Extract`
//! leaves) as Graphviz DOT — each output field's `Producer` is itself a DAG (`Match` rule table,
//! `Extract` leaf), and each `Extract`'s resolved `sanitize` chain is a small DAG of its own, so we
//! draw the sanitizer chain as a subgraph hanging off its `Extract` node rather than a separate
//! file.
//!
//! Usage: `plot_dag <topic-name> [-o <out-dir>]`, e.g. `plot_dag tilda/bikelanes -o dag_out`.
//! `<topic-name>` is the same string `TopicRunner::load` takes — `<config_root>/<topic-name>/`.
//!
//! One `.dot` file is written per output field per *distinct* resolved producer (topic default,
//! plus each category whose effective producer for that field differs) — categories sharing the
//! same producer for a field are grouped into one file rather than duplicated.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::PathBuf;

use anyhow::{Context, Result};

use osmnexus::lang::extract::Extract;
use osmnexus::lang::producer::Producer;
use osmnexus::lang::sanitize::Sanitizer;
use osmnexus::topic::runner::TopicRunner;

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let topic_name = args.next().context("usage: plot_dag <topic-name> [-o <out-dir>]")?;
    let mut out_dir = PathBuf::from("dag_out");
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-o" | "--out" => out_dir = PathBuf::from(args.next().context("-o needs a value")?),
            other => anyhow::bail!("unrecognized argument: {other}"),
        }
    }
    std::fs::create_dir_all(&out_dir)?;

    let runner = TopicRunner::load(&topic_name, 64, false)
        .with_context(|| format!("loading topic '{topic_name}'"))?;

    // field -> repr(producer) -> (one instance, labels of who produces it this way)
    let mut by_field: HashMap<String, HashMap<String, (&Producer, Vec<String>)>> = HashMap::new();
    for field in &runner.default_producers {
        let repr = format!("{:?}", field.source);
        by_field.entry(field.output.clone()).or_default()
            .entry(repr).or_insert((&field.source, Vec::new())).1.push("default".to_owned());
    }
    for (category, fields) in &runner.category_producers {
        for field in fields {
            let repr = format!("{:?}", field.source);
            by_field.entry(field.output.clone()).or_default()
                .entry(repr).or_insert((&field.source, Vec::new())).1.push(category.clone());
        }
    }

    let topic_slug = topic_name.replace('/', "_");
    let mut written = 0usize;
    for (field, variants) in &by_field {
        for (variant_idx, (producer, labels)) in variants.values().enumerate() {
            let mut labels = labels.clone();
            labels.sort();
            let suffix = if variants.len() == 1 {
                String::new()
            } else {
                format!("__v{variant_idx}")
            };
            let dot = render_dot(&topic_name, field, &labels, producer);
            let path = out_dir.join(format!("{topic_slug}__{field}{suffix}.dot"));
            std::fs::write(&path, dot)?;
            written += 1;
        }
    }

    eprintln!("wrote {written} DAG(s) to {}", out_dir.display());
    Ok(())
}

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

fn render_dot(topic: &str, field: &str, labels: &[String], producer: &Producer) -> String {
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
        "digraph dag {{\n  rankdir=TB;\n  node [fontname=\"sans-serif\", fontsize=11];\n  edge [fontname=\"sans-serif\", fontsize=9];\n{}}}\n",
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
                Extract::Value { key, .. } => format!("extract\nkey: {key}"),
                Extract::Candidates { keys, .. } => format!("extract\nkeys: {keys:?}"),
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
