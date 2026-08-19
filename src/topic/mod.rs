//! Loading and running one `topics/<name>/` directory against the generic `lang`/`categorize`
//! engines. Owns everything specific to *this project's* topic-directory convention (the
//! `topic.json` schema, the `{node,way,relation}/`+`macros.json`+`sanitizers.json`+`producers.json`
//! layout, and the per-element pipeline that sequences engine calls) — `lang`/`categorize`
//! themselves know nothing about any of this.
//!
//! - `spec`: the JSON schema types `topic.json`/`transforms.json` deserialize into
//!   (`TopicSpec`/`TransformsSpec`/`Field`/...).
//! - `load`: reading a topic's categories/macros/sanitizers/transforms off disk, following the
//!   `topics/<name>/...` directory convention.
//! - `runner`: `TopicRunner`, the fully loaded, ready-to-run topic — load-time orchestration
//!   (`load`/`load_all`) plus a thin per-element dispatch (`process`).
//! - `pipeline`: the actual per-element runtime pipeline (transform pipeline → `exclude_condition`
//!   → side-split → categorize → field evaluation), which `TopicRunner::process` delegates to.
//!
//! Geometry (row builders, primitives, relation resolution) lives in the top-level `geom` module,
//! not here — it's topic-independent, so it's kept out of the topic-loading/running engine.

pub mod inherit;
pub mod load;
pub mod pipeline;
pub mod runner;
pub mod spec;

pub use runner::TopicRunner;
