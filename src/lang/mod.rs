//! The generic, topic-agnostic extraction language: a JSON-rule tool for evaluating predicates
//! (`Filter`) and values (`Producer`) against an object's tags. Nothing here knows about the
//! `topics/<name>/{node,way,relation}/` directory convention or `topic.json` — that's
//! `crate::topic`, which loads a topic directory and drives one element through this engine — nor
//! about the category-matching machinery built on top of these primitives — that's
//! `crate::categorize`. See the `[[unbiased-engine-principle]]`: no tag-value/category/country
//! literals live here either — everything domain-specific is data, not Rust.
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
//! - `keys`: generic tag-key selection helpers (`first_present`) shared by `producer`
//!   and `filter`.

pub mod classifier;
pub mod extract;
pub mod filter;
pub mod keys;
pub mod producer;
pub mod sanitize;
