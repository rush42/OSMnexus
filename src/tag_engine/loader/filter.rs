//! Load-time resolution of a `Filter`: expanding `Macro` references and `sanitize:` names into
//! their fully-resolved form, so `filter::eval` never does a registry lookup.

use std::collections::HashMap;

use crate::tag_engine::producer::filter::Filter;
use crate::tag_engine::producer::{AtomicChain, SanitizeRef};

impl Filter {
    /// Recursively resolve every named reference this `Filter` (transitively) carries — `Macro`
    /// nodes (replaced by their expanded definition) and every `sanitize:` reference (resolved
    /// against `sanitizers`, `SanitizeRef::resolve`) — so `eval` never does a registry lookup of
    /// any kind. Called once at load time on every `Filter` a topic owns (category `condition`s,
    /// `exclude_condition`, and any `when`/`cond` embedded in a `Producer` — see
    /// `Producer::resolve`) against `macros`, the topic's raw (also possibly macro-referencing)
    /// macro definitions.
    ///
    /// Hard-errors on an undefined macro/sanitizer name or a cyclic macro definition (`A`
    /// referencing `B` referencing `A`) rather than infinite-recursing — the same
    /// fail-fast-at-load philosophy as `CategoriesFile::build_order`'s `excludes` cycle check.
    pub fn expand(
        &self,
        macros: &HashMap<String, Filter>,
        sanitizers: &HashMap<String, AtomicChain>,
    ) -> anyhow::Result<Filter> {
        self.expand_inner(macros, sanitizers, &mut Vec::new())
    }

    fn expand_inner(
        &self,
        macros: &HashMap<String, Filter>,
        sanitizers: &HashMap<String, AtomicChain>,
        stack: &mut Vec<String>,
    ) -> anyhow::Result<Filter> {
        let resolve = |s: &Option<SanitizeRef>| -> anyhow::Result<Option<SanitizeRef>> {
            s.as_ref().map(|r| r.resolve(sanitizers)).transpose()
        };
        Ok(match self {
            Filter::And { and } =>
                Filter::And { and: and.iter().map(|f| f.expand_inner(macros, sanitizers, stack)).collect::<anyhow::Result<_>>()? },
            Filter::Or { or } =>
                Filter::Or { or: or.iter().map(|f| f.expand_inner(macros, sanitizers, stack)).collect::<anyhow::Result<_>>()? },
            Filter::Not { not } =>
                Filter::Not { not: Box::new(not.expand_inner(macros, sanitizers, stack)?) },
            Filter::Macro { r#macro: name } => {
                if stack.iter().any(|n| n == name) {
                    stack.push(name.clone());
                    anyhow::bail!("cyclic macro definition: {}", stack.join(" -> "));
                }
                let def = macros.get(name)
                    .ok_or_else(|| anyhow::anyhow!("unknown macro: '{name}'"))?;
                stack.push(name.clone());
                let expanded = def.expand_inner(macros, sanitizers, stack)?;
                stack.pop();
                expanded
            }
            Filter::Bool(b) => Filter::Bool(*b),
            Filter::TagInSet { tag, in_set } =>
                Filter::TagInSet { tag: tag.clone(), in_set: in_set.clone() },
            Filter::TagIn { tag, r#in, sanitize } =>
                Filter::TagIn { tag: tag.clone(), r#in: r#in.clone(), sanitize: resolve(sanitize)? },
            Filter::TagContains { tag, contains, case_insensitive } =>
                Filter::TagContains { tag: tag.clone(), contains: contains.clone(), case_insensitive: *case_insensitive },
            Filter::TagStartsWith { tag, starts_with } =>
                Filter::TagStartsWith { tag: tag.clone(), starts_with: starts_with.clone() },
            Filter::TagEndsWith { tag, ends_with } =>
                Filter::TagEndsWith { tag: tag.clone(), ends_with: ends_with.clone() },
            Filter::TagExists { tag, exists } =>
                Filter::TagExists { tag: tag.clone(), exists: *exists },
            Filter::TagEq { tag, eq, sanitize } =>
                Filter::TagEq { tag: tag.clone(), eq: eq.clone(), sanitize: resolve(sanitize)? },
            Filter::FirstTagInSet { first_tag, in_set, sanitize } =>
                Filter::FirstTagInSet { first_tag: first_tag.clone(), in_set: in_set.clone(), sanitize: resolve(sanitize)? },
            Filter::FirstTagIn { first_tag, r#in, sanitize } =>
                Filter::FirstTagIn { first_tag: first_tag.clone(), r#in: r#in.clone(), sanitize: resolve(sanitize)? },
            Filter::FirstTagExists { first_tag, exists, sanitize } =>
                Filter::FirstTagExists { first_tag: first_tag.clone(), exists: *exists, sanitize: resolve(sanitize)? },
            Filter::ParentTagIn { parent_tag, r#in, sanitize } =>
                Filter::ParentTagIn { parent_tag: parent_tag.clone(), r#in: r#in.clone(), sanitize: resolve(sanitize)? },
            Filter::ParentTagContains { parent_tag, contains } =>
                Filter::ParentTagContains { parent_tag: parent_tag.clone(), contains: contains.clone() },
            Filter::ParentTagStartsWith { parent_tag, starts_with } =>
                Filter::ParentTagStartsWith { parent_tag: parent_tag.clone(), starts_with: starts_with.clone() },
            Filter::ParentTagEndsWith { parent_tag, ends_with } =>
                Filter::ParentTagEndsWith { parent_tag: parent_tag.clone(), ends_with: ends_with.clone() },
            Filter::ParentTagEq { parent_tag, eq, sanitize } =>
                Filter::ParentTagEq { parent_tag: parent_tag.clone(), eq: eq.clone(), sanitize: resolve(sanitize)? },
            Filter::Side { side } => Filter::Side { side: side.clone() },
            Filter::Prefix { prefix } => Filter::Prefix { prefix: prefix.clone() },
            Filter::Infix { infix } => Filter::Infix { infix: infix.clone() },
            Filter::HasKeyPrefix { has_key_prefix } => Filter::HasKeyPrefix { has_key_prefix: has_key_prefix.clone() },
            Filter::HasParent { has_parent } => Filter::HasParent { has_parent: *has_parent },
            Filter::NumLt { num, sanitize, lt } =>
                Filter::NumLt { num: num.clone(), sanitize: resolve(sanitize)?, lt: *lt },
            Filter::NumLte { num, sanitize, lte } =>
                Filter::NumLte { num: num.clone(), sanitize: resolve(sanitize)?, lte: *lte },
            Filter::NumGt { num, sanitize, gt } =>
                Filter::NumGt { num: num.clone(), sanitize: resolve(sanitize)?, gt: *gt },
            Filter::NumGte { num, sanitize, gte } =>
                Filter::NumGte { num: num.clone(), sanitize: resolve(sanitize)?, gte: *gte },
        })
    }
}
