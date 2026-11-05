//! Generic access to the named string-sets in `<config_root>/value_sets.json`.
//!
//! This module holds no domain knowledge: the sets are loaded once per compile from a JSON object
//! of `set-name → [values]` (`lang::env::CompileEnv`) and resolved by name at load time. What the sets *mean*
//! (which highways are allowed, which count as sidepaths, …) lives entirely in the
//! JSON and is referenced by name from Rust transforms and from category conditions
//! (the `tag … in_set` filter primitive).

use std::collections::HashSet;
use std::sync::Arc;

/// A value-set reference resolved at compile (load) time: the set itself plus its name for
/// display. `Filter::InSet` holds this instead of a bare name, so evaluating it is one `contains`
/// with no by-name lookup on the hot path (see `boundaries_plan.md`, step 5). Evaluation borrows the
/// `Arc` and never touches its count. Equality is by name. Resolving needs a compile environment
/// in scope (`lang::env::with_env`).
#[derive(Clone)]
pub struct ValueSetRef {
    pub name: String,
    pub set: Arc<HashSet<String>>,
}

impl ValueSetRef {
    pub fn resolve(name: impl Into<String>) -> anyhow::Result<Self> {
        let name = name.into();
        let set = crate::lang::env::current()?.value_set(&name)?;
        Ok(ValueSetRef { name, set })
    }
}

impl std::fmt::Debug for ValueSetRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("ValueSetRef").field(&self.name).finish()
    }
}

impl PartialEq for ValueSetRef {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name
    }
}
