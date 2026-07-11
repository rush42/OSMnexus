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
