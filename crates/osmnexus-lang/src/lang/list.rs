//! `ValueList`: the values of a `Filter::In` membership test, with the lookup strategy picked
//! once at load time — the same "decide at compile time, keep the hot path minimal" approach as
//! the decision tree. Short lists are scanned linearly (no hashing, cache-friendly); longer ones
//! get an `FxHashSet` as well.
//!
//! Measured on short OSM-style values (half hits, half misses, release build): a linear scan
//! costs ~1.5 ns at 1 entry and ~0.3 ns per extra entry, an `FxHashSet` lookup a flat ~3.3 ns.
//! They cross at 6–7 entries, hence [`HASH_THRESHOLD`].
//!
//! A list is either written inline (`"in": ["a", "b"]`) or named (`"in": "allowed_highway"`,
//! resolved against `lists.json` at load time). A named list keeps its name for display only;
//! evaluation never looks anything up by name.

use rustc_hash::FxHashSet;

/// Lists longer than this also get a hash set; up to this length, membership is a linear scan.
pub const HASH_THRESHOLD: usize = 6;

#[derive(Debug, Clone)]
pub struct ValueList {
    values: Vec<String>,
    set: Option<FxHashSet<String>>,
    name: Option<String>,
}

impl ValueList {
    /// An inline list, in the order it was written.
    pub fn new(values: Vec<String>) -> Self {
        let set = (values.len() > HASH_THRESHOLD).then(|| values.iter().cloned().collect());
        ValueList { values, set, name: None }
    }

    /// A named list from `lists.json`. `values` should already be sorted and deduplicated
    /// (`CompileEnv` does that), so the expression IR built from it is deterministic.
    pub fn named(name: impl Into<String>, values: Vec<String>) -> Self {
        ValueList { name: Some(name.into()), ..Self::new(values) }
    }

    /// Resolve a named list against the compile environment in scope (`lang::env::with_env`).
    pub fn resolve(name: &str) -> anyhow::Result<Self> {
        let values = crate::lang::env::current()?.list(name)?.to_vec();
        Ok(Self::named(name, values))
    }

    #[inline]
    pub fn contains(&self, value: &str) -> bool {
        match &self.set {
            Some(set) => set.contains(value),
            None => self.values.iter().any(|v| v == value),
        }
    }

    pub fn values(&self) -> &[String] {
        &self.values
    }

    /// The `lists.json` name, if this list was referenced by name.
    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    pub fn len(&self) -> usize {
        self.values.len()
    }

    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }
}

impl PartialEq for ValueList {
    fn eq(&self, other: &Self) -> bool {
        self.values == other.values && self.name == other.name
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list(n: usize) -> ValueList {
        ValueList::new((0..n).map(|i| format!("v{i}")).collect())
    }

    #[test]
    fn strategy_switches_at_threshold() {
        assert!(list(HASH_THRESHOLD).set.is_none());
        assert!(list(HASH_THRESHOLD + 1).set.is_some());
    }

    #[test]
    fn both_strategies_agree() {
        for n in [0, 1, HASH_THRESHOLD, HASH_THRESHOLD + 1, 32] {
            let l = list(n);
            for i in 0..n + 3 {
                assert_eq!(l.contains(&format!("v{i}")), i < n, "n={n} i={i}");
            }
            assert!(!l.contains(""));
        }
    }
}
