//! Emit a topic's output `Producer` trees, or its categorization trees, as JSON node/edge graphs
//! (see `osmnexus::dag`) — the live editor's backend spawns this to feed its browser-rendered tree
//! views. Same field/variant grouping as `bin/plot_dag` (which emits Graphviz DOT instead, deriver
//! trees only), just JSON on stdout rather than one `.dot` file per variant.
//!
//! Usage: `dag_json <config-dir> <topic-name> [category|decision-tree]`, e.g. `dag_json
//! configs/tilda tilda/bikelanes`. `<topic-name>` is the same string `TopicRunner::load` takes —
//! `<config-dir>/<topic-name>/`. Pass `category` as the third argument to get the per-`ElementKind`
//! categorization trees (the flat, human-readable priority order which category an object is
//! assigned) instead of the default per-field deriver trees (how an already-classified object's
//! output values are computed); pass `decision-tree` to instead get the *compiled* discrimination
//! net (`decision_tree::DecisionTree`) that actually prunes the runtime walk.

use std::collections::HashMap;

use anyhow::{Context, Result};
use serde::Serialize;

use osmnexus::dag::{category_order_dag, decision_tree_dag, producer_dag, DagGraph};
use osmnexus::lang::producer::Producer;
use osmnexus::topic::runner::TopicRunner;

#[derive(Serialize)]
struct Variant {
    labels: Vec<String>,
    #[serde(flatten)]
    graph: DagGraph,
}

#[derive(Serialize)]
struct Response {
    topic: String,
    fields: HashMap<String, Vec<Variant>>,
}

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let config_dir = args.next().context("usage: dag_json <config-dir> <topic-name> [category|decision-tree]")?;
    let topic_name = args.next().context("usage: dag_json <config-dir> <topic-name> [category|decision-tree]")?;
    let mode = args.next();
    let category_mode = mode.as_deref() == Some("category");
    let decision_tree_mode = mode.as_deref() == Some("decision-tree");

    osmnexus::paths::set_config_root(config_dir);
    let runner = TopicRunner::load(&topic_name, 64)
        .with_context(|| format!("loading topic '{topic_name}'"))?;

    let fields = if decision_tree_mode {
        // Same per-kind, single-variant shape as `category_mode` below, just the compiled tree
        // instead of the flat priority list.
        runner.categories.iter()
            .map(|(kind, cats)| {
                let variant = Variant { labels: vec![kind.id_prefix().to_owned()], graph: decision_tree_dag(cats) };
                (kind.id_prefix().to_owned(), vec![variant])
            })
            .collect()
    } else if category_mode {
        // "field" here is the element kind ("node"/"way"/"relation") — one tree per kind the topic
        // has any categories for, single variant each (a categorization tree has no per-category
        // dedup the way a field's producer does).
        runner.categories.iter()
            .map(|(kind, cats)| {
                let variant = Variant { labels: vec![kind.id_prefix().to_owned()], graph: category_order_dag(cats) };
                (kind.id_prefix().to_owned(), vec![variant])
            })
            .collect()
    } else {
        // field -> repr(producer) -> (one instance, labels of who produces it this way) — same
        // dedup `bin/plot_dag` does, so categories sharing a field's producer collapse into one
        // variant.
        let mut by_field: HashMap<String, HashMap<String, (&Producer, Vec<String>)>> = HashMap::new();
        for field in &runner.default_outputs {
            let repr = format!("{:?}", field.source);
            by_field.entry(field.output.clone()).or_default()
                .entry(repr).or_insert((&field.source, Vec::new())).1.push("default".to_owned());
        }
        for (category, fields) in &runner.category_outputs {
            for field in fields {
                let repr = format!("{:?}", field.source);
                by_field.entry(field.output.clone()).or_default()
                    .entry(repr).or_insert((&field.source, Vec::new())).1.push(category.clone());
            }
        }

        by_field.into_iter()
            .map(|(field, variants)| {
                let mut variants: Vec<Variant> = variants.into_values()
                    .map(|(producer, mut labels)| {
                        labels.sort();
                        Variant { labels, graph: producer_dag(producer) }
                    })
                    .collect();
                variants.sort_by(|a, b| a.labels.cmp(&b.labels));
                (field, variants)
            })
            .collect()
    };

    println!("{}", serde_json::to_string(&Response { topic: topic_name, fields })?);
    Ok(())
}
