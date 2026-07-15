//! The generic, topic-agnostic tag-rule engine: a JSON-rule tool for evaluating predicates
//! (`Filter`) and values (`Producer`) against an object's tags, plus the category-matching
//! machinery (`categories`/`decision_tree`) built on top of them. Nothing here knows about the
//! `topics/<name>/{node,way,relation}/` directory convention or `topic.json` — that's
//! `crate::topic`, which loads a topic directory and drives one element through this engine.
//! See the `[[unbiased-engine-principle]]`: no tag-value/category/country literals live here
//! either — everything domain-specific is data, not Rust.
//!
//! Each module owns one concept end-to-end, both its load-time reference resolution
//! (macro/sanitizer expansion) and its runtime evaluation, rather than splitting those across
//! separate directories:
//! - `filter`: the `Filter` predicate AST, `expand` (macro/sanitize resolution), and `eval`.
//! - `producer`: the `Producer` value engine — really just `Match`/`Extract` (`fallback` is
//!   JSON-only sugar for a `Match`; a named shared classifier table is inlined as JSON by
//!   `topic::load` before `Producer` ever sees it), its `resolve`, and its context
//!   (`ExtractCtx`/`TagSet`) and result (`Produced`) types.
//! - `extract`: `Extract`, the "read a raw tag value, optionally sanitized" logic shared by every
//!   `Filter::Tag*` predicate and `Producer::Extract` — factored out so it's written once.
//! - `sanitize`: the atomic `&str -> atomic` chain machinery (`SanitizeRef`/`Sanitizer`/`Step`)
//!   underneath an `Extract`'s `sanitize:` field, plus the one built-in, `parse_length`.
//! - `classifier`: the generic first-match-wins rule table underneath `Producer::Match`.
//! - `parser`: hand-written `Deserialize` impls folding JSON-only sugar (`Producer`'s `fallback`;
//!   `Sanitizer`'s bare-single-step shape; `Step`'s `cases`/`filter`/`drop`) into each type's own
//!   canonical form — kept separate from the runtime types/eval logic it parses into.
//! - `categories`: the category data model, its runtime first-match evaluator, and the load-time
//!   compiler (`build_order`) of a category set's `excludes` relation into a priority order.
//! - `decision_tree`: the discrimination net that prunes `categorize`'s first-match walk, plus its
//!   load-time compiler.
//! - `transform`: `InputTransform` (one in-place tag-mutation step), `TransformStep`/`CloneStep`
//!   (the object-cardinality-changing wrapper around it — side-splitting today), and the two
//!   dynamic-key-iteration helpers (`unnest_prefixed_tags`, `strip_prefix`) a single `Producer`
//!   output can't express.
//! - `keys`: generic tag-key selection helpers (`first_present`) shared by `producer`
//!   and `filter`.
//! - `linter`: the category-overlap lint — compiles a `Filter` to a boolean `Expr`
//!   (`filter_to_expr`/`to_nnf`) and checks same-priority categories for satisfiable overlap.

pub mod categories;
pub mod classifier;
pub mod decision_tree;
pub mod extract;
pub mod filter;
pub mod keys;
pub mod linter;
mod parser;
pub mod producer;
pub mod sanitize;
pub mod transform;
