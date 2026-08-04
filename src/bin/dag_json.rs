//! Emit a topic's output `Producer` trees, or its categorization trees, as JSON node/edge graphs
//! (see `osmnexus::dag`) — the live editor's backend spawns this to feed its browser-rendered tree
//! views. Same field/variant grouping as `bin/plot_dag` (which emits Graphviz DOT instead, deriver
//! trees only), just JSON on stdout rather than one `.dot` file per variant.
//!
//! Usage: `dag_json <config-dir> <topic-name> <mode> <selector> [order-idx]`, e.g. `dag_json
//! configs tilda/bikelanes deriver list` or `dag_json configs tilda/bikelanes deriver surface`.
//! `<topic-name>` is the same string `TopicRunner::load` takes — `<config-dir>/topics/<topic-name>/`.
//! `<mode>` is `deriver` (per-field output producer trees), `category` (one category's own
//! condition + what it excludes, picked from its kind's priority order), `decision-tree` (the
//! *compiled* discrimination net that actually prunes the runtime walk), or `sanitizer` (one named
//! sanitizer's own mapping/replace/builtin chain, shared+topic-local merged the same way
//! `TopicRunner::load` resolves `sanitize:` references — see `load_topic_sanitizers`). `<selector>`
//! is either `list` — cheap, no graphs built, just the available field/kind/sanitizer names for the
//! picker — or one of those names. For `deriver`/`decision-tree`/`sanitizer`, a name alone builds
//! that field's/kind's/sanitizer's graph. `category` has a third level: a kind name alone (no
//! `order-idx`) returns that kind's category names in priority order (still no graph built) instead
//! of one crammed-together tree of every category at once; passing `order-idx` too (an index into
//! that ordered list) builds the graph for just that one category's condition. Building every
//! field/kind's graph on every request (the live editor only ever displays one at a time) was the
//! dominant cost for topics with many/large fields, so the caller is expected to fetch `list` once
//! and then one graph per field the user actually selects.

use anyhow::{bail, ensure, Context, Result};
use serde::Serialize;

use osmnexus::tree::{self, category_condition_dag, decision_tree_dag, producer_dag, sanitizer_dag, DagGraph};
use osmnexus::lang::producer::Producer;
use osmnexus::topic::load::load_topic_sanitizers;
use osmnexus::topic::runner::TopicRunner;

#[derive(Serialize)]
struct Variant {
    labels: Vec<String>,
    #[serde(flatten)]
    graph: DagGraph,
}

#[derive(Serialize)]
struct ListResponse {
    topic: String,
    names: Vec<String>,
}

#[derive(Serialize)]
struct GraphResponse {
    topic: String,
    name: String,
    variants: Vec<Variant>,
}

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let usage = "usage: dag_json <config-dir> <topic-name> <deriver|category|decision-tree|sanitizer> <list|name> [order-idx]";
    let config_dir = args.next().context(usage)?;
    let topic_name = args.next().context(usage)?;
    let mode = args.next().context(usage)?;
    let selector = args.next().context(usage)?;
    let order_idx: Option<usize> = args.next().map(|s| s.parse()).transpose().context("order-idx must be a number")?;

    osmnexus::paths::set_config_root(config_dir);

    // `sanitizer` mode never needs a fully-loaded `TopicRunner` — `load_topic_sanitizers` is the
    // same shared+topic-local merge `TopicRunner::load` itself does internally (see its own doc),
    // called here directly instead of paying for the whole topic's categories/producers/transforms
    // to be compiled just to read back the one map it doesn't otherwise expose.
    if mode == "sanitizer" {
        let base = osmnexus::paths::config_root().join(&topic_name);
        let config_root = base.parent().expect("topics/<name> has a parent").to_path_buf();
        let sanitizers = load_topic_sanitizers(&base, &config_root)
            .with_context(|| format!("loading sanitizers for topic '{topic_name}'"))?;
        if selector == "list" {
            let mut names: Vec<String> = sanitizers.keys().cloned().collect();
            names.sort();
            println!("{}", serde_json::to_string(&ListResponse { topic: topic_name, names })?);
            return Ok(());
        }
        let chain = sanitizers.get(&selector).with_context(|| format!("unknown sanitizer '{selector}'"))?;
        let variant = Variant { labels: Vec::new(), graph: sanitizer_dag(&selector, chain) };
        let resp = GraphResponse { topic: topic_name, name: selector, variants: vec![variant] };
        println!("{}", serde_json::to_string(&resp)?);
        return Ok(());
    }

    // Compiling the per-kind decision tree (`CategoriesFile::build_order`'s `decision_tree::build`
    // call, skipped via `TopicRunner::load`'s `linear_classify` flag) is by far the most expensive
    // part of loading a topic — and it's only ever read by `decision_tree_dag` below, for an actual
    // (non-`list`) decision-tree request. Every other mode/selector, including the decision-tree
    // view's own field-name `list`, never looks at `cats.tree`, so there's no reason to pay for it.
    let build_tree = mode == "decision-tree" && selector != "list";
    let runner = TopicRunner::load(&topic_name, 64, !build_tree)
        .with_context(|| format!("loading topic '{topic_name}'"))?;

    match mode.as_str() {
        "decision-tree" => {
            if selector == "list" {
                let names = runner.categories.keys().map(|k| k.id_prefix().to_owned()).collect();
                println!("{}", serde_json::to_string(&ListResponse { topic: topic_name, names })?);
                return Ok(());
            }
            let (kind, cats) = runner.categories.iter().find(|(k, _)| k.id_prefix() == selector)
                .with_context(|| format!("unknown kind '{selector}'"))?;
            let variant = Variant { labels: vec![kind.id_prefix().to_owned()], graph: decision_tree_dag(cats) };
            let resp = GraphResponse { topic: topic_name, name: selector, variants: vec![variant] };
            println!("{}", serde_json::to_string(&resp)?);
        }
        "category" => {
            if selector == "list" {
                let names = runner.categories.keys().map(|k| k.id_prefix().to_owned()).collect();
                println!("{}", serde_json::to_string(&ListResponse { topic: topic_name, names })?);
                return Ok(());
            }
            let (_, cats) = runner.categories.iter().find(|(k, _)| k.id_prefix() == selector)
                .with_context(|| format!("unknown kind '{selector}'"))?;
            match order_idx {
                // No category picked yet — just its kind's category names, in the same priority
                // order `order` (and therefore the runtime first-match walk) already uses, so the
                // picker reflects the actual evaluation order rather than an arbitrary one.
                None => {
                    let names = (0..cats.order.len()).map(|i| tree::order_label(cats, i)).collect();
                    println!("{}", serde_json::to_string(&ListResponse { topic: topic_name, names })?);
                }
                Some(idx) => {
                    ensure!(idx < cats.order.len(), "order index {idx} out of range for kind '{selector}'");
                    let label = tree::order_label(cats, idx);
                    let variant = Variant { labels: vec![label.clone()], graph: category_condition_dag(cats, idx) };
                    let resp = GraphResponse { topic: topic_name, name: label, variants: vec![variant] };
                    println!("{}", serde_json::to_string(&resp)?);
                }
            }
        }
        "deriver" => {
            if selector == "list" {
                // Field names only — no `Producer` repr formatting, no `producer_dag` walks, so this
                // stays cheap even when some field's tree is huge.
                let mut names: Vec<String> = runner.default_outputs.iter().map(|f| f.output.clone())
                    .chain(runner.category_outputs.values().flatten().map(|f| f.output.clone()))
                    .collect();
                names.sort();
                names.dedup();
                println!("{}", serde_json::to_string(&ListResponse { topic: topic_name, names })?);
                return Ok(());
            }
            // Same dedup `bin/plot_dag` does: producers sharing a repr for this field collapse into
            // one variant, labeled by who produces it that way — but only for the requested field.
            let mut by_repr: std::collections::HashMap<String, (&Producer, Vec<String>)> = std::collections::HashMap::new();
            for field in runner.default_outputs.iter().filter(|f| f.output == selector) {
                let repr = format!("{:?}", field.source);
                by_repr.entry(repr).or_insert((&field.source, Vec::new())).1.push("default".to_owned());
            }
            for (category, fields) in &runner.category_outputs {
                for field in fields.iter().filter(|f| f.output == selector) {
                    let repr = format!("{:?}", field.source);
                    by_repr.entry(repr).or_insert((&field.source, Vec::new())).1.push(category.clone());
                }
            }
            if by_repr.is_empty() {
                bail!("unknown field '{selector}'");
            }
            let mut variants: Vec<Variant> = by_repr.into_values()
                .map(|(producer, mut labels)| {
                    labels.sort();
                    Variant { labels, graph: producer_dag(producer) }
                })
                .collect();
            variants.sort_by(|a, b| a.labels.cmp(&b.labels));
            let resp = GraphResponse { topic: topic_name, name: selector, variants };
            println!("{}", serde_json::to_string(&resp)?);
        }
        other => bail!("unknown mode '{other}' ({usage})"),
    }
    Ok(())
}
