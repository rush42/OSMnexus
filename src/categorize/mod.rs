//! The category-matching layer, built on top of the `crate::lang` extraction primitives: given a
//! set of categories (each a named `Filter` predicate with a priority/excludes relation), decide
//! which category an object falls into. Nothing here knows about the `topics/<name>/...` directory
//! convention or `topic.json` — that's `crate::topic`, which loads a topic directory and drives one
//! element through this machinery. See the `[[unbiased-engine-principle]]`: no
//! tag-value/category/country literals live here either — everything domain-specific is data, not
//! Rust.
//!
//! Each module owns one concept end-to-end, both its load-time reference resolution and its runtime
//! evaluation, rather than splitting those across separate directories:
//! - `categories`: the category data model, its runtime first-match evaluator, and the load-time
//!   compiler (`build_order`) of a category set's `excludes` relation into a priority order — uses
//!   `crate::decision_tree` (a top-level module, not nested here: `lang::producer::Producer::Match`
//!   uses the same discrimination-net engine, so it isn't category-specific).
//! - `transform`: `InputTransform` (one in-place tag-mutation step), `TransformStep`/`CloneStep`
//!   (the object-cardinality-changing wrapper around it — side-splitting today), and the two
//!   dynamic-key-iteration helpers (`unnest_prefixed_tags`, `strip_prefix`) a single `Producer`
//!   output can't express.
//! - `linter`: the category-overlap lint — compiles a `Filter` to a boolean `Expr`
//!   (`filter_to_expr`/`to_nnf`) and checks same-priority categories for satisfiable overlap.

pub mod categories;
pub mod linter;
pub mod transform;
