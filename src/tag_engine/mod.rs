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
//! - `sanitize`: the atomic `&str -> atomic` chain machinery (`SanitizeRef`/`AtomicChain`/`Step`)
//!   underneath an `Extract`'s `sanitize:` field, plus the one built-in, `parse_length`.
//! - `classifier`: the generic first-match-wins rule table underneath `Producer::Match`.
//! - `categories`: the category data model, its runtime first-match evaluator, and the load-time
//!   compiler (`build_order`) of a category set's `excludes` relation into a priority order.
//! - `decision_tree`: the discrimination net that prunes `categorize`'s first-match walk, plus its
//!   load-time compiler.
//! - `input_transforms`: `InputTransform`, the runtime in-place tag-mutation step.
//! - `transform`: object-cardinality-changing steps (center-line side-split) and `strip_prefix` —
//!   the operations needing dynamic key iteration a single `Producer` output can't express.
//! - `keys`: generic tag-key selection helpers (`first_present`/`sided_keys`) shared by `producer`
//!   and `filter`.

pub mod categories;
pub mod classifier;
pub mod decision_tree;
pub mod filter;
pub mod input_transforms;
pub mod keys;
pub mod producer;
pub mod sanitize;
pub mod transform;
