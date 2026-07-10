//! Load-time resolution of a `Producer`: substituting every named reference (macros, sanitizer
//! names, shared classifiers) it transitively carries with its fully-resolved form, so
//! `Producer::eval` never does a registry lookup.

use std::collections::HashMap;

use crate::tag_engine::producer::filter::Filter;
use crate::tag_engine::producer::{AtomicChain, Producer, SanitizeRef};

impl SanitizeRef {
    /// `name` not found in `sanitizers` falls back to a built-in alias (`Step::Builtin`, e.g.
    /// `"parse_length"`) rather than erroring — mirrors the pre-inlining fallback
    /// (`None => apply_builtin(name, raw)`). A truly unrecognized name still isn't caught until
    /// `apply_builtin` runs (it has no load-time name registry of its own, just one built-in), so
    /// it warns-and-drops per row rather than failing to load — the same looseness the built-in
    /// fallback always had.
    pub(crate) fn resolve(&self, sanitizers: &HashMap<String, AtomicChain>) -> anyhow::Result<SanitizeRef> {
        match self {
            SanitizeRef::Name(name) => Ok(SanitizeRef::Inline(match sanitizers.get(name) {
                Some(chain) => chain.clone(),
                None => AtomicChain::One(crate::tag_engine::producer::Step::Builtin(name.clone())),
            })),
            SanitizeRef::Inline(_) => Ok(self.clone()),
        }
    }
}

fn resolve_opt_sanitize(
    sanitize: &Option<SanitizeRef>,
    sanitizers: &HashMap<String, AtomicChain>,
) -> anyhow::Result<Option<SanitizeRef>> {
    sanitize.as_ref().map(|r| r.resolve(sanitizers)).transpose()
}

impl Producer {
    /// Resolve every named reference this producer (transitively) carries, once, at load time:
    /// macros embedded in a `Classify`'s `rules[].when` or a `Cond`'s `cond` (`Filter::expand`),
    /// `Extract`'s `sanitize:` (`SanitizeRef::resolve`), and `SharedClassify` (inlined into an
    /// equivalent `Classify` — its rules go through the same macro/sanitize resolution as any
    /// other `Classify`'s). After this, `eval` never does a registry lookup of any kind.
    pub fn resolve(
        &self,
        macros: &HashMap<String, Filter>,
        sanitizers: &HashMap<String, AtomicChain>,
    ) -> anyhow::Result<Producer> {
        Ok(match self {
            Producer::Fallback { fallback } => Producer::Fallback {
                fallback: fallback.iter().map(|p| p.resolve(macros, sanitizers)).collect::<anyhow::Result<_>>()?,
            },
            Producer::Classify { rules, default, from, consts } => Producer::Classify {
                rules: rules.iter()
                    .map(|r| Ok(crate::tag_engine::producer::classifier::Rule {
                        when: r.when.expand(macros, sanitizers)?,
                        value: r.value.clone(),
                    }))
                    .collect::<anyhow::Result<_>>()?,
                default: default.clone(),
                from: *from,
                consts: consts.clone(),
            },
            Producer::SharedClassify { shared, from, consts } => {
                let classifier = crate::tag_engine::loader::classifier::shared_classifier(shared);
                let rules = classifier.rules.iter()
                    .map(|r| Ok(crate::tag_engine::producer::classifier::Rule {
                        when: r.when.expand(macros, sanitizers)?,
                        value: r.value.clone(),
                    }))
                    .collect::<anyhow::Result<_>>()?;
                Producer::Classify {
                    rules,
                    default: classifier.default.clone(),
                    from: *from,
                    consts: consts.clone(),
                }
            }
            Producer::Cond { cond, then, r#else } => Producer::Cond {
                cond: cond.expand(macros, sanitizers)?,
                then: Box::new(then.resolve(macros, sanitizers)?),
                r#else: r#else.as_ref().map(|p| p.resolve(macros, sanitizers)).transpose()?.map(Box::new),
            },
            Producer::Extract { key, keys, from, side, sanitize, consts, directed } => Producer::Extract {
                key: key.clone(),
                keys: keys.clone(),
                from: *from,
                side: side.clone(),
                sanitize: resolve_opt_sanitize(sanitize, sanitizers)?,
                consts: consts.clone(),
                directed: *directed,
            },
        })
    }
}
