//! The tag-processing engine: how a way's raw OSM tags become one topic's output rows.
//!
//! - `topic`/`topic_runner`: JSON schema (`TopicSpec`) and the loaded, ready-to-run `TopicRunner`.
//! - `runner`: the per-element pipeline (`pre_cat_steps` → `exclude_condition` → `transform` →
//!   categorize → `producer` field evaluation).
//! - `producer`: the `Producer` engine (`Extract`/`Fallback`/`Cond`/`Classify`/`SharedClassify`/
//!   `Atomic`) that evaluates one field's value — shared by `osm_fields`, sanitizers, and derivers
//!   alike; all three are just JSON shorthands over this one type, loaded into one
//!   `derivers.json`-plus-`sanitizers.json` registry. No Rust derivers remain
//!   (`traffic_mode`/`smoothness_parent` were datafied via `Producer::Cond`; `smoothness_parent`'s
//!   exact behavior was dropped rather than reimplemented — see project memory backlog for how to
//!   bring it back as data). Atomic `&str -> atomic` chain steps (`Step`, data-defined, plus the
//!   one built-in, `parse_length`) live here too, as the building blocks of `Producer::Atomic`.
//! - `classifier`/`filter`: the generic first-match-wins rule table and predicate AST underneath
//!   `Producer::Classify`, category matching, and `exclude_condition`.
//! - `categories`/`decision_tree`/`loader`: the category data model, its priority-order pruning
//!   net, and its JSON loading.
//! - `keys`: generic tag-key selection helpers (`first_present`/`sided_keys`) shared by
//!   `producer` and `filter`.
//! - `transform`: object-cardinality-changing steps (center-line side-split) — the one thing that
//!   isn't a per-object field evaluation.

pub mod categories;
pub mod classifier;
pub mod decision_tree;
pub mod filter;
pub mod keys;
pub mod loader;
pub mod producer;
pub mod runner;
pub mod topic;
pub mod topic_runner;
pub mod transform;
