//! `osmnexus-codegen`: views of a compiled topic (see `boundaries_plan.md`, step 9). Depends only
//! on `osmnexus-lang`: everything here reads a loaded `Topic` (or its inferred `OutputSchema`) and
//! produces text or JSON, with no file I/O of its own — the binaries in the `osmnexus` crate read
//! the config and write the results.
//!
//! - `ts`: TypeScript types per output field (`export_ts_types`).
//! - `dot`: Graphviz DOT of a field's producer tree (`plot_tree`).
//! - `tree`: the node/edge graphs behind the live editor's tree views (`tree_json`).

#![forbid(unsafe_code)]

use osmnexus_lang::topic::Topic;

pub mod dot;
pub mod tree;
pub mod ts;

/// A whole-topic output format: one generated document per topic. New formats (TileJSON, JSON
/// Schema, Mermaid docs) implement this; per-query views like `tree` don't.
pub trait Backend {
    /// File extension for the generated document, without the dot.
    fn file_extension(&self) -> &'static str;
    /// Generate the document for `topic`, named `topic_name` in the output.
    fn generate(&self, topic_name: &str, topic: &Topic) -> String;
}
