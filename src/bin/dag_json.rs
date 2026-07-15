//! Emit a topic's output `Producer` trees as JSON node/edge graphs (see `osmnexus::dag`) — the
//! live editor's backend spawns this to feed its browser-rendered tree view. Same field/variant
//! grouping as `bin/plot_dag` (which emits Graphviz DOT instead), just JSON on stdout rather than
//! one `.dot` file per variant.
//!
//! Usage: `dag_json <config-dir> <topic-name>`, e.g. `dag_json configs/tilda tilda/bikelanes`.
//! `<topic-name>` is the same string `TopicRunner::load` takes — `<config-dir>/<topic-name>/`.

use std::collections::HashMap;

use anyhow::{Context, Result};
use serde::Serialize;

use osmnexus::dag::{producer_dag, DagGraph};
use osmnexus::tag_engine::producer::Producer;
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
    let config_dir = args.next().context("usage: dag_json <config-dir> <topic-name>")?;
    let topic_name = args.next().context("usage: dag_json <config-dir> <topic-name>")?;

    osmnexus::paths::set_config_root(config_dir);
    let runner = TopicRunner::load(&topic_name, 64)
        .with_context(|| format!("loading topic '{topic_name}'"))?;

    // field -> repr(producer) -> (one instance, labels of who produces it this way) — same dedup
    // `bin/plot_dag` does, so categories sharing a field's producer collapse into one variant.
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

    let fields = by_field.into_iter()
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
        .collect();

    println!("{}", serde_json::to_string(&Response { topic: topic_name, fields })?);
    Ok(())
}
