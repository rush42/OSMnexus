//! The generic, topic-agnostic extraction language: a JSON-rule tool for evaluating predicates
//! (`Filter`) and values (`Producer`) against an object's tags. Nothing here knows about the
//! `topics/<name>/{node,way,relation}/` directory convention or `topic.json` — that's
//! `crate::topic`, which loads a topic directory and drives one element through this engine — nor
//! about the category-matching machinery built on top of these primitives — that's
//! `crate::categorize`. See the `[[unbiased-engine-principle]]`: no tag-value/category/country
//! literals live here either — everything domain-specific is data, not Rust.
//!
//! Each module owns one concept end-to-end, both its runtime evaluation and (where it needs one)
//! its own JSON sugar, rather than splitting those across separate directories. A named reference
//! — a macro, a `sanitize: "<name>"`, a shared classifier table — is never a concept any of these
//! types carries at all: `topic::load`'s `inline_macro_refs`/`inline_sanitize_refs`/
//! `inline_shared_producers` resolve all three as a `serde_json::Value`-tree rewrite before any of
//! this module's `Deserialize` impls ever run, so there's only one (resolved) tier of each type.
//! - `filter`: the `Filter` predicate AST and `eval`.
//! - `producer`: the `Producer` value engine — really just `Match`/`Extract` (`fallback` is
//!   JSON-only sugar for a `Match`), its `eval`, its context (`ExtractCtx`/`TagSet`) and result
//!   (`Produced`) types, and the generic first-match-wins rule table (`Rule`/`match_rules`)
//!   underneath `Producer::Match`.
//! - `extract`: `Extract`, the "read a raw tag value, optionally sanitized" logic shared by every
//!   `Filter`'s tag/num predicates and `Producer::Extract` — factored out so it's written once.
//! - `sanitize`: the atomic `&str -> atomic` chain machinery (`Sanitizer`/`Step`) underneath an
//!   `Extract`'s `sanitize:` field, plus the one built-in, `parse_length`.

pub mod extract;
pub mod filter;
pub mod producer;
pub mod sanitize;
