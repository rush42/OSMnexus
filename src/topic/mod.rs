//! Loading and running one `topics/<name>/` directory. The compile side (schema, loading,
//! `Topic`, output schema, `ConfigSources`) lives in the `osmnexus-lang` crate and is re-exported
//! here, so `crate::topic::{spec, load, model, schema, sources, Topic}` keep working; the run side
//! is local:
//!
//! - `pipeline`: the per-element runtime pipeline (transform pipeline → `exclude_condition` →
//!   side-split → categorize → field evaluation), entered through `pipeline::process`.
//! - `inherit`: relation → member field inheritance.
//!
//! Geometry (row builders, primitives, relation resolution) lives in the top-level `geom` module.

pub use osmnexus_lang::topic::*;

pub mod inherit;
pub mod pipeline;
