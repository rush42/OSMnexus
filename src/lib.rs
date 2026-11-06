//! `osmnexus`: the imperative shell around `osmnexus-lang` — OSM reading, geometry, the
//! per-element pipeline, outputs and the CLI. The language crate's modules are re-exported under
//! their old paths (`crate::lang`, `crate::categorize`, …) so existing call sites and downstream
//! users keep working.

pub use osmnexus_lang::{categorize, decision_tree, lang, units};
pub use osmnexus_codegen as codegen;
pub use osmnexus_codegen::tree;

pub mod config;
pub mod db;
pub mod geom;
pub mod csv_source;
pub mod osm;
pub mod output;
pub mod config_dir;
pub mod paths;
pub mod processing;
pub mod topic;
