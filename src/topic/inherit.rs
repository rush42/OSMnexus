//! `inherit_to_member`: handing a kept relation's exported fields down to its member ways.
//!
//! The point of doing this here rather than at output-build time is ordering. A relation wanting
//! graph-shaped output used to be the primary row, reaching out at output time to *borrow* geometry
//! from its member ways' edges — and a relation's member way ids are an arbitrary set scattered
//! across a different pass's output, so that lookup needed the one random-access hashmap left in the
//! join (`output::cursor::group_edges_by_way`). Flipping it round removes the need: a member way
//! carries its relation-derived context in its *own* tag row, and every remaining geometry join is
//! the clean "this element's own geometry" case.
//!
//! The timing works because the relations pass runs *before* Pass A (`osm::reader::sorted`'s module
//! doc). Relations are classified, their exported fields captured here, and only then are ways
//! classified — so every parent a member way could have is already known when the way is processed.
//!
//! Writes happen from the relations pass's parallel classify closures and reads from Pass A's, so
//! the per-topic maps sit behind an `RwLock`. In practice the write side is tiny (one insert per
//! kept relation — 665 on `configs/public_transport` over bremen) and the read side takes a shared
//! lock only for ways that actually have a parent.

use std::sync::{Arc, RwLock};

use rustc_hash::FxHashMap;
use serde_json::Value;

use crate::osm::types::RawTags;
use crate::topic::spec::InheritMode;
use crate::topic::TopicRunner;

/// One parent relation's exported fields, extracted once at relation-classify time and shared by
/// every member way that inherits them.
///
/// A `RawTags` rather than a JSON map, because that is the shape the extraction context already
/// speaks: the member way sees these as its `ExtractCtx::parent_tags`, so a way topic reads them
/// with the *existing* `{"parent": {"tag": "ref"}}` / `parent_or_obj` producer syntax, and can match
/// conditions and categories on them too. Nothing new was added to the producer language for this.
pub type ExportedFields = Arc<RawTags<'static>>;

/// Per-topic `relation osm_id -> exported fields`. `None` for a topic that declares no
/// `inherit_to_member`, so a config that doesn't use the feature allocates nothing and pays no
/// lookup.
pub struct InheritStore {
    per_topic: Vec<Option<RwLock<FxHashMap<i64, ExportedFields>>>>,
}

impl InheritStore {
    pub fn new(runners: &[TopicRunner]) -> Self {
        InheritStore {
            per_topic: runners
                .iter()
                .map(|r| r.spec.inherit_to_member.as_ref().map(|_| RwLock::new(FxHashMap::default())))
                .collect(),
        }
    }

    /// Whether any topic declares `inherit_to_member` — gates building the reverse
    /// way → relation index at all (see `osm::reader`), which is the only part of this feature with
    /// a size that scales with the extract.
    pub fn any(&self) -> bool {
        self.per_topic.iter().any(Option::is_some)
    }

    /// Extract topic `idx`'s declared fields from a just-classified relation's `produced` and retain
    /// them for its member ways. `produced` is the relation's own pre-serialized JSON; a declared
    /// field the relation didn't produce is simply absent (its outputs are category-dependent).
    pub fn capture(&self, runners: &[TopicRunner], idx: usize, rel_osm_id: i64, produced: &str) {
        let Some(slot) = self.per_topic.get(idx).and_then(Option::as_ref) else { return };
        let Some(spec) = runners[idx].spec.inherit_to_member.as_ref() else { return };
        if produced.is_empty() {
            return;
        }
        let Ok(Value::Object(map)) = serde_json::from_str::<Value>(produced) else { return };
        let mut exported = RawTags::default();
        for field in &spec.fields {
            if let Some(v) = map.get(field) {
                exported.insert(field.clone().into(), value_to_tag(v).into());
            }
        }
        if exported.is_empty() {
            return;
        }
        slot.write().unwrap().insert(rel_osm_id, Arc::new(exported));
    }

    fn get(&self, idx: usize, rel_osm_id: i64) -> Option<ExportedFields> {
        self.per_topic.get(idx)?.as_ref()?.read().unwrap().get(&rel_osm_id).cloned()
    }

    /// This topic's parent tagsets for a member way, in `parents` order (sorted ids — see
    /// `osm::reader::build_way_parents`), skipping parents that exported nothing.
    ///
    /// Empty when the topic declares no inheritance or the way has no exporting parent, which is
    /// what `build_topic_rows` treats as "emit exactly one row with no parent" — so a member way
    /// whose parents all exported nothing behaves exactly like a non-member way.
    pub fn parents_for(&self, idx: usize, parents: &[i64]) -> Vec<(i64, ExportedFields)> {
        if parents.is_empty() || self.per_topic.get(idx).map_or(true, Option::is_none) {
            return Vec::new();
        }
        parents.iter().filter_map(|&id| self.get(idx, id).map(|f| (id, f))).collect()
    }

    /// This topic's `mode`, or `None` if it declares no inheritance.
    pub fn mode(&self, runners: &[TopicRunner], idx: usize) -> Option<InheritMode> {
        runners[idx].spec.inherit_to_member.as_ref().map(|s| s.mode)
    }
}

/// A relation's produced value as a tag string. `produced` holds real JSON (numbers, bools), while
/// tags are text — `Value::String` unwraps rather than serializing, so `"6"` stays `6` and not
/// `"\"6\""`.
fn value_to_tag(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Fold several parents into one tagset for `InheritMode::Merge`, later parent winning on collision.
pub fn merge_parents(parents: &[(i64, ExportedFields)]) -> RawTags<'static> {
    let mut merged = RawTags::default();
    for (_, fields) in parents {
        for (k, v) in fields.iter() {
            merged.insert(k.clone(), v.clone());
        }
    }
    merged
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn produced_values_become_plain_tag_text_not_json_literals() {
        assert_eq!(value_to_tag(&Value::String("6".into())), "6");
        assert_eq!(value_to_tag(&serde_json::json!(0)), "0");
        assert_eq!(value_to_tag(&serde_json::json!(true)), "true");
    }

    fn fields(pairs: &[(&str, &str)]) -> ExportedFields {
        Arc::new(pairs.iter().map(|(k, v)| ((*k).to_owned().into(), (*v).to_owned().into())).collect())
    }

    #[test]
    fn merge_lets_the_later_parent_win_on_collision() {
        let merged = merge_parents(&[(1, fields(&[("ref", "6"), ("route", "tram")])), (2, fields(&[("ref", "8")]))]);
        assert_eq!(merged.get("ref").map(|c| c.as_ref()), Some("8"));
        assert_eq!(merged.get("route").map(|c| c.as_ref()), Some("tram"));
    }
}
