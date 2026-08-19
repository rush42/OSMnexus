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
use serde_json::{Map, Value};

use crate::output::rows::TopicRow;
use crate::topic::spec::InheritMode;
use crate::topic::TopicRunner;

/// One parent relation's exported fields, extracted once at relation-classify time and shared by
/// every member way that inherits them.
pub type ExportedFields = Arc<Map<String, Value>>;

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
        let mut exported = Map::new();
        for field in &spec.fields {
            if let Some(v) = map.get(field) {
                exported.insert(field.clone(), v.clone());
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

    /// Rewrite one topic's rows for a member way, per that topic's `mode`. `parents` is the way's
    /// parent relation ids in the order the relations pass saw them, so the result is a
    /// deterministic function of blob order like everything else on the ordered path.
    ///
    /// Returns `rows` untouched when the topic declares no inheritance, the way has no parents, or
    /// no parent exported anything — including for `FanOut`, so a member way whose parents all
    /// exported nothing keeps exactly one row rather than vanishing.
    pub fn apply(
        &self,
        runners: &[TopicRunner],
        idx: usize,
        parents: &[i64],
        rows: Vec<TopicRow>,
    ) -> Vec<TopicRow> {
        if rows.is_empty() || parents.is_empty() {
            return rows;
        }
        let Some(spec) = runners[idx].spec.inherit_to_member.as_ref() else { return rows };
        let found: Vec<(i64, ExportedFields)> =
            parents.iter().filter_map(|&id| self.get(idx, id).map(|f| (id, f))).collect();
        if found.is_empty() {
            return rows;
        }
        match spec.mode {
            InheritMode::Merge => rows
                .into_iter()
                .map(|mut row| {
                    let merged: Map<String, Value> =
                        found.iter().flat_map(|(_, f)| f.iter()).map(|(k, v)| (k.clone(), v.clone())).collect();
                    row.produced = merge_into_produced(&row.produced, merged.iter());
                    row
                })
                .collect(),
            // One row per (row, parent). Rows for one way stay contiguous, which is what the output
            // cursors' repeated-`osm_id` caching needs (`OrderedGeomCursor::last`) — the same shape
            // side-split already produces.
            InheritMode::FanOut => {
                let mut out = Vec::with_capacity(rows.len() * found.len());
                for row in &rows {
                    for (rel_id, fields) in &found {
                        let mut cloned = clone_row(row);
                        cloned.produced = merge_into_produced(&row.produced, fields.iter());
                        if let Some(id) = &mut cloned.id {
                            id.push_str(&format!("/relation/{rel_id}"));
                        }
                        out.push(cloned);
                    }
                }
                out
            }
        }
    }
}

/// `TopicRow` isn't `Clone` (nothing else needs it — every other producer of rows builds them once
/// and hands them straight to a sink), so fan-out clones explicitly here.
fn clone_row(row: &TopicRow) -> TopicRow {
    TopicRow {
        osm_id: row.osm_id,
        osm_type: row.osm_type,
        id: row.id.clone(),
        category: row.category.clone(),
        produced: row.produced.clone(),
        annotations: row.annotations.clone(),
        meta: row.meta.clone(),
    }
}

/// Merge inherited fields into a member's own pre-serialized `produced`. The member's own values
/// win: a way's own `name` is about the way, and must not be overwritten by its route's.
///
/// This parses and reprints `produced`, which is exactly what the output path was just changed to
/// stop doing — but here it runs on the parallel classify workers and only for ways that actually
/// have an exporting parent, and the alternative (splicing text) would emit unsorted keys, breaking
/// the sorted-key shape every other row has.
fn merge_into_produced<'a>(
    produced: &str,
    inherited: impl Iterator<Item = (&'a String, &'a Value)>,
) -> String {
    let mut map: Map<String, Value> = match serde_json::from_str(produced) {
        Ok(Value::Object(m)) => m,
        _ => Map::new(),
    };
    for (k, v) in inherited {
        map.entry(k.clone()).or_insert_with(|| v.clone());
    }
    serde_json::to_string(&Value::Object(map)).unwrap_or_else(|_| produced.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fields(pairs: &[(&str, &str)]) -> ExportedFields {
        Arc::new(pairs.iter().map(|(k, v)| ((*k).to_owned(), Value::String((*v).to_owned()))).collect())
    }

    #[test]
    fn inherited_fields_do_not_overwrite_the_members_own_values() {
        let out = merge_into_produced(
            r#"{"name":"Hauptstraße","highway":"secondary"}"#,
            fields(&[("name", "Tram 6"), ("ref", "6")]).iter(),
        );
        // The way's own `name` survives; the parent's `ref` is added.
        assert_eq!(out, r#"{"highway":"secondary","name":"Hauptstraße","ref":"6"}"#);
    }

    #[test]
    fn merged_output_keeps_keys_sorted_like_every_other_row() {
        let out = merge_into_produced(r#"{"z":"1"}"#, fields(&[("a", "2"), ("m", "3")]).iter());
        assert_eq!(out, r#"{"a":"2","m":"3","z":"1"}"#);
    }

    #[test]
    fn an_empty_or_malformed_produced_still_receives_the_inherited_fields() {
        assert_eq!(merge_into_produced("", fields(&[("ref", "6")]).iter()), r#"{"ref":"6"}"#);
        assert_eq!(merge_into_produced("not json", fields(&[("ref", "6")]).iter()), r#"{"ref":"6"}"#);
    }
}
