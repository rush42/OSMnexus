//! `osmnexus-lang`: the functional core of osmnexus (see `boundaries_plan.md`).
//!
//! Everything here is tag-level and pure: parse and validate topic declarations from
//! [`topic::sources::ConfigSources`] (a config directory as data), resolve macros, sanitizer and
//! shared-producer references and value sets against a [`lang::env::CompileEnv`], compile category
//! rules into decision trees, and evaluate the result against an element's tags. No file I/O, no
//! process-global state, no OSM reading, geometry or output — those live in the `osmnexus` crate,
//! which depends on this one (never the other way round).

#![forbid(unsafe_code)]

pub mod categorize;
pub mod decision_tree;
pub mod lang;
pub mod topic;
pub mod units;
pub mod value_sets;

#[cfg(test)]
pub(crate) mod test_support;
