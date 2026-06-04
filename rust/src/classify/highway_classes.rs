//! Highway-class membership sets, datafied to `topics/_shared/highway_classes.json`.
//!
//! The named groups (motorway / major_road / minor_road / path) are loaded once at first use.
//! The pipeline only ever consults two sets, composed from those groups:
//!   - [`allowed_highways`]         — every known highway value (all groups). Backs the
//!     `is_allowed_highway` exclusion gate and the lifecycle construction swap.
//!   - [`sidepath_highway_classes`] — `path` ∪ {pedestrian}; the highways whose bare
//!     `cycleway:*` tags get unnested onto the self object during side splitting.
//!
//! Motorway classes are *included* in `allowed_highways`: they are valid highways that the
//! roads/barrierLines topics categorize. Motorway-specific handling lives in those topics.

use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

/// The raw named groups, parsed from the shared JSON and cached for the process lifetime.
fn groups() -> &'static HashMap<String, Vec<String>> {
    static GROUPS: OnceLock<HashMap<String, Vec<String>>> = OnceLock::new();
    GROUPS.get_or_init(|| {
        const PATH: &str =
            concat!(env!("CARGO_MANIFEST_DIR"), "/topics/_shared/highway_classes.json");
        let raw = std::fs::read_to_string(PATH)
            .unwrap_or_else(|e| panic!("reading {PATH}: {e}"));
        serde_json::from_str(&raw).unwrap_or_else(|e| panic!("parsing {PATH}: {e}"))
    })
}

/// Iterate a named group, panicking if the JSON is missing it (load-time data error).
fn group(name: &str) -> impl Iterator<Item = &'static str> {
    groups()
        .get(name)
        .unwrap_or_else(|| panic!("highway_classes.json missing group '{name}'"))
        .iter()
        .map(String::as_str)
}

/// Every known highway value (the union of all groups).
pub fn allowed_highways() -> &'static HashSet<&'static str> {
    static ALLOWED: OnceLock<HashSet<&'static str>> = OnceLock::new();
    ALLOWED.get_or_init(|| groups().values().flatten().map(String::as_str).collect())
}

/// `path` classes plus `pedestrian` — used for sidepath detection / bare-`cycleway` unnesting.
pub fn sidepath_highway_classes() -> &'static HashSet<&'static str> {
    static SIDEPATH: OnceLock<HashSet<&'static str>> = OnceLock::new();
    SIDEPATH.get_or_init(|| {
        let mut s: HashSet<&'static str> = group("path").collect();
        s.insert("pedestrian");
        s
    })
}
