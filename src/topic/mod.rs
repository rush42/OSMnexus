//! Loading and running one `topics/<name>/` directory against the generic `tag_engine`. Owns
//! everything specific to *this project's* topic-directory convention (the `topic.json` schema,
//! the `{node,way,relation}/`+`macros.json`+`sanitizers.json`+`derivers.json` layout, and the
//! per-element pipeline that sequences engine calls) — `tag_engine` itself knows nothing about any
//! of this.
//!
//! - `spec`: the JSON schema types `topic.json` deserializes into (`TopicSpec`/`Field`/...).
//! - `load`: reading a topic's categories/macros/sanitizers off disk, following the
//!   `topics/<name>/...` directory convention.
//! - `runner`: `TopicRunner`, the fully loaded, ready-to-run topic — load-time orchestration
//!   (`load`/`load_all`) plus a thin per-element dispatch (`process`).
//! - `pipeline`: the actual per-element runtime pipeline (`pre_cat_steps` → `exclude_condition` →
//!   side-split → categorize → field evaluation), which `TopicRunner::process` delegates to.

pub mod load;
pub mod pipeline;
pub mod runner;
pub mod spec;

pub use runner::TopicRunner;
