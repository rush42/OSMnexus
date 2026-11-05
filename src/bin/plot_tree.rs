//! Plot a topic's output `Producer` trees (and the sanitizer chains hanging off their `Extract`
//! leaves) as Graphviz DOT — each output field's `Producer` walks out to a tree (`Match` rule
//! table, `Extract` leaf), and each `Extract`'s resolved `sanitize` chain to a small tree of its own,
//! so we
//! draw the sanitizer chain as a subgraph hanging off its `Extract` node rather than a separate
//! file.
//!
//! Usage: `plot_tree <topic-name> [-o <out-dir>]`, e.g. `plot_tree tilda/bikelanes -o tree_out`.
//! `<topic-name>` is the same string `Topic::load` takes — `<config_root>/<topic-name>/`.
//!
//! One `.dot` file is written per output field per *distinct* resolved producer (topic default,
//! plus each category whose effective producer for that field differs) — categories sharing the
//! same producer for a field are grouped into one file rather than duplicated.

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{Context, Result};

use osmnexus::lang::producer::Producer;

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let topic_name = args.next().context("usage: plot_tree <topic-name> [-o <out-dir>]")?;
    let mut out_dir = PathBuf::from("tree_out");
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-o" | "--out" => out_dir = PathBuf::from(args.next().context("-o needs a value")?),
            other => anyhow::bail!("unrecognized argument: {other}"),
        }
    }
    std::fs::create_dir_all(&out_dir)?;

    // `0` skips compiling both the category decision tree and each producer Match's own compiled
    // tree — neither is ever read here (this binary only plots producer/sanitizer trees from their
    // raw `rules`/chain data, never `CategoriesFile::tree`; see `tree.rs`'s `Producer::Match` arm,
    // which explicitly ignores its compiled `tree` field), so there's nothing to lose skipping it.
    // Config root: `TILDA_CONFIG_DIR` or the default (see `paths::config_root`).
    let config = osmnexus::config_dir::Config::default_root()?;
    let runner = config.load(&topic_name, 0)
        .with_context(|| format!("loading topic '{topic_name}'"))?;

    // field -> repr(producer) -> (one instance, labels of who produces it this way)
    let mut by_field: HashMap<String, HashMap<String, (&Producer, Vec<String>)>> = HashMap::new();
    for field in &runner.default_producers {
        let repr = format!("{:?}", field.source);
        by_field.entry(field.output.clone()).or_default()
            .entry(repr).or_insert((&field.source, Vec::new())).1.push("default".to_owned());
    }
    // `category_producers` is indexed by `CategoryDef::idx`, so recover each slot's name from the
    // category set it came from.
    let mut category_names: Vec<String> = vec![String::new(); runner.category_producers.len()];
    for cats in runner.categories.values() {
        for cat in &cats.categories {
            category_names[cat.idx] = cat.id.clone();
        }
    }
    for (idx, fields) in runner.category_producers.iter().enumerate() {
        let category = &category_names[idx];
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
            let dot = osmnexus::codegen::dot::render_dot(&topic_name, field, &labels, producer);
            let path = out_dir.join(format!("{topic_slug}__{field}{suffix}.dot"));
            std::fs::write(&path, dot)?;
            written += 1;
        }
    }

    eprintln!("wrote {written} tree(s) to {}", out_dir.display());
    Ok(())
}
