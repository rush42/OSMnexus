//! `CompileEnv`: every input to compilation that isn't a topic's own files — the shared
//! named value lists, unit tables and the traffic side. Built once by the shell and passed to
//! `Topic::load`; replaces the former process-globals (`value_sets`/`units` `OnceLock`s reading
//! `<config_root>/…json`, `traffic::LEFT_HAND_TRAFFIC`), so two configs can be compiled in one
//! process and a function's real inputs are visible (see `boundaries_plan.md`, step 6).
//!
//! Compilation resolves every reference into the IR (`Filter::In` holds its list's values, the
//! `parse_length` builtin holds its unit table, `DirectedExtract` its keys), so evaluation never
//! needs the env. It's only consulted while deserializing, which is why it's made available to the
//! hand-written `Deserialize` impls through [`with_env`]: serde's derive can't thread a context
//! parameter through nested types. The scope is per thread and per call, not process-global —
//! nothing outside `with_env` can observe it, and topic deserialization never runs on rayon
//! workers (only tree building, after deserialization, does).

use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap};
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use anyhow::Context;

use crate::topic::sources::ConfigSources;

/// A unit-conversion table (`[(suffix, factor), ...]`, see `units::parse_compound_unit`), shared.
/// Compared and hashed by value (factors by bit pattern) so it can sit inside `Sanitizer`, which
/// derives `Eq`/`Hash`/`Ord`.
#[derive(Debug, Clone)]
pub struct UnitTable(pub Arc<[(String, f32)]>);

impl UnitTable {
    fn key(&self) -> impl Iterator<Item = (&str, u32)> {
        self.0.iter().map(|(s, f)| (s.as_str(), f.to_bits()))
    }
}

impl PartialEq for UnitTable {
    fn eq(&self, other: &Self) -> bool {
        self.key().eq(other.key())
    }
}
impl Eq for UnitTable {}
impl Hash for UnitTable {
    fn hash<H: Hasher>(&self, state: &mut H) {
        for k in self.key() {
            k.hash(state);
        }
    }
}
impl PartialOrd for UnitTable {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for UnitTable {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.key().cmp(other.key())
    }
}

#[derive(Debug, Default)]
pub struct CompileEnv {
    /// Named value lists (`lists.json`), each sorted and deduplicated.
    lists: HashMap<String, Vec<String>>,
    units: HashMap<String, UnitTable>,
    pub left_hand_traffic: bool,
}

impl CompileEnv {
    /// Read `lists.json` and `units.json` from the config root (each optional — a missing
    /// file just means no lists / tables, and referencing one is then a load error).
    pub fn from_sources(sources: &ConfigSources, left_hand_traffic: bool) -> anyhow::Result<Self> {
        let lists: HashMap<String, BTreeSet<String>> = match sources.read("lists.json") {
            Some(raw) => serde_json::from_str(raw)
                .with_context(|| format!("parsing {}", sources.path("lists.json").display()))?,
            None => HashMap::new(),
        };
        let units: HashMap<String, Vec<(String, f32)>> = match sources.read("units.json") {
            Some(raw) => serde_json::from_str(raw)
                .with_context(|| format!("parsing {}", sources.path("units.json").display()))?,
            None => HashMap::new(),
        };
        Ok(CompileEnv {
            lists: lists.into_iter().map(|(k, v)| (k, v.into_iter().collect())).collect(),
            units: units.into_iter().map(|(k, v)| (k, UnitTable(v.into()))).collect(),
            left_hand_traffic,
        })
    }

    /// A named value list, sorted and deduplicated.
    pub fn list(&self, name: &str) -> anyhow::Result<&[String]> {
        self.lists
            .get(name)
            .map(Vec::as_slice)
            .with_context(|| format!("lists.json: no list named '{name}'"))
    }

    pub fn unit_table(&self, name: &str) -> anyhow::Result<UnitTable> {
        self.units
            .get(name)
            .cloned()
            .with_context(|| format!("units.json: no unit table named '{name}'"))
    }
}

thread_local! {
    static CURRENT: RefCell<Option<Arc<CompileEnv>>> = const { RefCell::new(None) };
}

/// Run `f` with `env` as the compile environment for this thread, restoring the previous one
/// afterwards (also on panic), so nested calls and other threads are unaffected.
pub fn with_env<R>(env: &Arc<CompileEnv>, f: impl FnOnce() -> R) -> R {
    struct Restore(Option<Arc<CompileEnv>>);
    impl Drop for Restore {
        fn drop(&mut self) {
            CURRENT.with(|c| *c.borrow_mut() = self.0.take());
        }
    }
    let _restore = Restore(CURRENT.with(|c| c.borrow_mut().replace(env.clone())));
    f()
}

/// The compile environment of the enclosing [`with_env`] call. Errors outside one: something is
/// deserializing topic IR without going through `Topic::load` (or a test forgot to set one up).
pub fn current() -> anyhow::Result<Arc<CompileEnv>> {
    CURRENT
        .with(|c| c.borrow().clone())
        .context("no compile environment in scope (deserialize topic config inside lang::env::with_env)")
}
