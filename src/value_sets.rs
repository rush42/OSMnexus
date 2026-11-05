//! Generic access to the named string-sets in `<config_root>/value_sets.json`.
//!
//! This module holds no domain knowledge: it loads a JSON object of
//! `set-name → [values]` once, and hands back a set by name. What the sets *mean*
//! (which highways are allowed, which count as sidepaths, …) lives entirely in the
//! JSON and is referenced by name from Rust transforms and from category conditions
//! (the `tag … in_set` filter primitive).

use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

fn all_sets() -> &'static HashMap<String, HashSet<String>> {
    static SETS: OnceLock<HashMap<String, HashSet<String>>> = OnceLock::new();
    SETS.get_or_init(|| {
        let path = crate::paths::config_root().join("value_sets.json");
        let raw = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
        serde_json::from_str(&raw).unwrap_or_else(|e| panic!("parsing {}: {e}", path.display()))
    })
}

/// The named set, panicking if the JSON doesn't define it (a config error, surfaced loudly).
pub fn value_set(name: &str) -> &'static HashSet<String> {
    all_sets()
        .get(name)
        .unwrap_or_else(|| panic!("value_sets.json: no set named '{name}'"))
}

/// Like [`value_set`], but `None` instead of panicking — for compile-time resolution, where an
/// unknown name becomes a load error.
pub fn try_value_set(name: &str) -> Option<&'static HashSet<String>> {
    all_sets().get(name)
}

/// A value-set reference resolved at compile (load) time: the set itself plus its name for
/// display. `Filter::InSet` holds this instead of a bare name, so evaluating it is one `contains`
/// with no by-name lookup on the hot path (see `boundaries_plan.md`, step 5). Equality is by name.
#[derive(Clone)]
pub struct ValueSetRef {
    pub name: String,
    pub set: &'static HashSet<String>,
}

impl ValueSetRef {
    pub fn resolve(name: impl Into<String>) -> anyhow::Result<Self> {
        let name = name.into();
        match try_value_set(&name) {
            Some(set) => Ok(ValueSetRef { name, set }),
            None => anyhow::bail!("value_sets.json: no set named '{name}'"),
        }
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
