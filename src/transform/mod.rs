pub mod lifecycle;
pub mod side_split;

use std::collections::BTreeMap;

use crate::osm::types::RawTags;

/// A tag-rewriting transform applied in-place before categorization. Built from a topic's
/// `transforms` list. `lifecycle` is the one remaining named (no-arg) native transform; the rest
/// are data-driven primitives (`rename_key`, `value_cases`, `strip_prefix`) parametrized from JSON,
/// so bikelane/OSM vocabulary lives in the topic definition, not in this binary.
///
/// Center-line splitting (`split_sides`) is *not* here: it changes object cardinality and is handled
/// separately by `side_split`.
#[derive(Debug, Clone)]
pub enum TagTransform {
    /// `highway=construction` normalization + access-restriction lifecycle heuristics.
    /// Native because its branches share an early-return and its access predicate / free-text scan
    /// are irreducible control flow (see `src/transform/lifecycle.rs`).
    Lifecycle,

    /// Move one key to another, optionally gated on the source value. On match: remove `from`,
    /// insert `to` with the (preserved) value. Replaces the old `cycleway_both` transform, e.g.
    /// `{ "transform": "rename_key", "from": "cycleway", "to": "cycleway:both", "when_value": "no" }`.
    RenameKey {
        from: String,
        to: String,
        when_value: Option<String>,
    },

    /// Match one tag's value against a case table; the matched case contributes a set of tag writes,
    /// and (if `remove_tag`) the source tag is removed. Replaces `cycleway_opposite`, e.g.
    /// `{ "transform": "value_cases", "tag": "cycleway", "remove_tag": true,
    ///    "cases": { "opposite_lane": { "cycleway:left": "lane", "oneway:bicycle": "no" }, ... } }`.
    ValueCases {
        tag: String,
        remove_tag: bool,
        cases: BTreeMap<String, BTreeMap<String, String>>,
    },

    /// For every key starting with `prefix`, strip it, re-key the value onto the base tag, and stamp
    /// a marker. The marker key is `<base>:<stamp_key>` when the base starts with one of
    /// `stamp_nested_under`, else `stamp_key`. Replaces `construction_prefix`, e.g.
    /// `{ "transform": "strip_prefix", "prefix": "construction:", "stamp_key": "lifecycle",
    ///    "stamp_value": "construction", "stamp_nested_under": ["cycleway:", "sidewalk:"] }`.
    StripPrefix {
        prefix: String,
        stamp_key: String,
        stamp_value: String,
        stamp_nested_under: Vec<String>,
    },
}

impl TagTransform {
    pub fn apply(&self, tags: &mut RawTags) {
        match self {
            TagTransform::Lifecycle => {
                crate::profile::time(&crate::profile::LIFECYCLE, || {
                    lifecycle::transform_lifecycle_tags(tags);
                });
            }

            TagTransform::RenameKey { from, to, when_value } => {
                if let Some(v) = tags.get(from) {
                    let hit = when_value.as_deref().map_or(true, |w| w == v);
                    if hit {
                        let v = v.clone();
                        tags.remove(from);
                        tags.insert(to.clone(), v);
                    }
                }
            }

            TagTransform::ValueCases { tag, remove_tag, cases } => {
                if let Some(v) = tags.get(tag).cloned() {
                    if let Some(writes) = cases.get(&v) {
                        if *remove_tag {
                            tags.remove(tag);
                        }
                        for (k, val) in writes {
                            tags.insert(k.clone(), val.clone());
                        }
                    }
                }
            }

            TagTransform::StripPrefix { prefix, stamp_key, stamp_value, stamp_nested_under } => {
                let matched: Vec<(String, String)> = tags
                    .iter()
                    .filter(|(k, _)| k.starts_with(prefix.as_str()))
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect();

                for (key, value) in matched {
                    let base = key[prefix.len()..].to_owned();
                    tags.insert(base.clone(), value);
                    tags.remove(&key);

                    let marker = if stamp_nested_under.iter().any(|p| base.starts_with(p.as_str())) {
                        format!("{base}:{stamp_key}")
                    } else {
                        stamp_key.clone()
                    };
                    tags.insert(marker, stamp_value.clone());
                }
            }
        }
    }
}
