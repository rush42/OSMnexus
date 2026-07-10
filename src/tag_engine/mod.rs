//! The tag-processing engine: how a way's raw OSM tags become one topic's output rows.
//!
//! Split into two directory modules along a load-time/runtime boundary:
//!
//! - `producer`: pure per-object evaluation — the `Producer` engine (`Extract`/`Fallback`/`Cond`/
//!   `Classify`/`SharedClassify`), the `Filter` predicate evaluator, the category discrimination
//!   net walk, and the per-element pipeline (`pre_cat_steps` → `exclude_condition` → `transform` →
//!   categorize → field evaluation). No disk I/O, no name lookups — every macro/sanitizer/
//!   shared-classifier reference has already been resolved by the time anything here runs.
//! - `loader`: JSON parsing plus resolving every named reference into that ready-to-run form —
//!   `Filter::expand`, `Producer::resolve`, the category `excludes`-relation compiler, and
//!   `TopicRunner::load`, which orchestrates all of it into one loaded topic.

pub mod loader;
pub mod producer;
