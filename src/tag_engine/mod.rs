//! The tag-processing engine: how a way's raw OSM tags become one topic's output rows.
//!
//! - `topic`/`topic_runner`: JSON schema (`TopicSpec`) and the loaded, ready-to-run `TopicRunner`.
//! - `runner`: the per-element pipeline (`pre_cat_steps` → `exclude_condition` → `transform` →
//!   categorize → `producer` field evaluation).
//! - `producer`: the `Producer` engine (`Extract`/`Fallback`/`Classify`/`SharedClassify`/`Derive`)
//!   that evaluates one field's value — shared by `osm_fields`, sanitizers, and derivers alike.
//! - `classifier`/`filter`: the generic first-match-wins rule table and predicate AST underneath
//!   `Producer::Classify`, category matching, and `exclude_condition`.
//! - `categories`/`decision_tree`/`loader`: the category data model, its priority-order pruning
//!   net, and its JSON loading.
//! - `sanitize`: atomic `&str -> atomic` cleanup chains (data-defined, plus the one built-in,
//!   `parse_length`).
//! - `derive`: the two Rust derivers that need cross-object state a `Producer` can't express
//!   (`traffic_mode`, `smoothness_parent`).
//! - `keys`: generic tag-key selection helpers (`first_present`/`sided_keys`) shared by
//!   `producer`, `filter`, and `derive` — not sanitizer-specific, despite once living there.
//! - `transform`: object-cardinality-changing steps (center-line side-split) — the one thing that
//!   isn't a per-object field evaluation.

pub mod categories;
pub mod classifier;
pub mod decision_tree;
pub mod derive;
pub mod filter;
pub mod keys;
pub mod loader;
pub mod producer;
pub mod runner;
pub mod sanitize;
pub mod topic;
pub mod topic_runner;
pub mod transform;
