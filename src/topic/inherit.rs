//! `inherit_to_member`: handing a kept relation's exported fields down to its member ways.
//!
//! The point of doing this here rather than at output-build time is ordering. A relation wanting
//! graph-shaped output used to be the primary row, reaching out at output time to *borrow* geometry
//! from its member ways' edges — and a relation's member way ids are an arbitrary set scattered
//! across a different pass's output, so that lookup needed a random-access hashmap in the join.
//! Flipping it round removed the need (and that fallback is gone): a member way carries its
//! relation-derived context in its *own* tag row — including the relation's id, read as
//! `{"parent": {"key": "@id"}}` — and every geometry join is the element's own geometry.
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

use crate::osm::types::{RawTags, RelData};
use crate::topic::spec::InheritMode;
use crate::topic::Topic;

/// What one kept relation hands its member ways, recorded once at relation-classify time and
/// shared by every member way that inherits from it.
///
/// `fields` is a `RawTags` rather than a JSON map, because that is the shape the extraction context
/// already speaks: the member way sees it as its `ExtractCtx::parent_tags`, so a way topic reads it
/// with the existing `{"parent": {"key": "ref"}}` producer syntax and can match on it too. The
/// relation's own facts come along for `@timestamp`/`@role` (`@id` is the map key).
pub struct ParentRecord {
    pub fields: RawTags<'static>,
    pub timestamp: Option<i64>,
    /// `(member way id, role)`, sorted by way id. A way listed twice keeps its first role.
    roles: Box<[(i64, Arc<str>)]>,
}

impl ParentRecord {
    fn role_of(&self, way_id: i64) -> Option<&Arc<str>> {
        let i = self.roles.partition_point(|(w, _)| *w < way_id);
        self.roles.get(i).filter(|(w, _)| *w == way_id).map(|(_, r)| r)
    }
}

/// One parent of one member way, as `build_topic_rows` consumes it.
#[derive(Clone)]
pub struct Parent {
    pub rel_id: i64,
    pub record: Arc<ParentRecord>,
    /// This way's role in that relation — `None` only if the relation no longer lists it.
    pub role: Option<Arc<str>>,
}

/// Per-topic `relation osm_id -> record`. `None` for a topic that declares no `inherit_to_member`,
/// so a config that doesn't use the feature allocates nothing and pays no lookup.
pub struct InheritStore {
    per_topic: Vec<Option<RwLock<FxHashMap<i64, Arc<ParentRecord>>>>>,
}

impl InheritStore {
    pub fn new(runners: &[Topic]) -> Self {
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

    /// Record a just-classified relation of topic `idx` as a parent for its member ways: its
    /// declared fields extracted from its `produced` (its own pre-serialized JSON; a declared field
    /// it didn't produce is simply absent), its timestamp, and each member way's role.
    ///
    /// Every kept relation is recorded, even one that exports no field at all: the parent's identity
    /// is information in its own right — a member way reads it as `{"parent": {"key": "@id"}}` — so
    /// "member of relation 7" must not collapse into "not a member" just because relation 7 had no
    /// `ref`. `fields` may be empty for a topic that only wants membership.
    pub fn capture(&self, runners: &[Topic], idx: usize, rel: &RelData<'_>, produced: &str) {
        let Some(slot) = self.per_topic.get(idx).and_then(Option::as_ref) else { return };
        let Some(spec) = runners[idx].spec.inherit_to_member.as_ref() else { return };
        let mut fields = RawTags::default();
        if !spec.fields.is_empty() && !produced.is_empty() {
            if let Ok(Value::Object(map)) = serde_json::from_str::<Value>(produced) {
                for field in &spec.fields {
                    if let Some(v) = map.get(field) {
                        fields.insert(field.clone().into(), value_to_tag(v).into());
                    }
                }
            }
        }
        let mut roles: Vec<(i64, Arc<str>)> = rel
            .member_ways
            .iter()
            .zip(&rel.member_role_names)
            .map(|(&(way_id, _), &role)| (way_id, intern_role(role)))
            .collect();
        roles.sort_by_key(|(w, _)| *w); // stable: a way listed twice keeps its first role
        roles.dedup_by_key(|(w, _)| *w);
        let record = ParentRecord { fields, timestamp: rel.meta.timestamp, roles: roles.into_boxed_slice() };
        slot.write().unwrap().insert(rel.id, Arc::new(record));
    }

    fn get(&self, idx: usize, rel_osm_id: i64) -> Option<Arc<ParentRecord>> {
        self.per_topic.get(idx)?.as_ref()?.read().unwrap().get(&rel_osm_id).cloned()
    }

    /// Way `way_id`'s parents for topic `idx`, in `parents` order (sorted ids — see
    /// `osm::reader::build_way_parents`), for every parent this topic kept.
    ///
    /// Empty when the topic declares no inheritance or none of the way's parents was kept by it,
    /// which is what `build_topic_rows` treats as "emit exactly one row with no parent".
    pub fn parents_for(&self, idx: usize, way_id: i64, parents: &[i64]) -> Vec<Parent> {
        if parents.is_empty() || self.per_topic.get(idx).map_or(true, Option::is_none) {
            return Vec::new();
        }
        parents
            .iter()
            .filter_map(|&rel_id| {
                let record = self.get(idx, rel_id)?;
                let role = record.role_of(way_id).cloned();
                Some(Parent { rel_id, record, role })
            })
            .collect()
    }

    /// This topic's `mode`, or `None` if it declares no inheritance.
    pub fn mode(&self, runners: &[Topic], idx: usize) -> Option<InheritMode> {
        runners[idx].spec.inherit_to_member.as_ref().map(|s| s.mode)
    }
}

/// Role strings are a handful of distinct values (`""`, `forward`, `platform`, …) repeated across
/// every membership, so each thread keeps one shared copy per distinct role.
fn intern_role(role: &str) -> Arc<str> {
    thread_local! {
        static ROLES: std::cell::RefCell<FxHashMap<Box<str>, Arc<str>>> = std::cell::RefCell::new(FxHashMap::default());
    }
    ROLES.with(|roles| {
        let mut roles = roles.borrow_mut();
        if let Some(r) = roles.get(role) {
            return r.clone();
        }
        let r: Arc<str> = Arc::from(role);
        roles.insert(role.into(), r.clone());
        r
    })
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
pub fn merge_parents(parents: &[Parent]) -> RawTags<'static> {
    let mut merged = RawTags::default();
    for p in parents {
        for (k, v) in p.record.fields.iter() {
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

    fn parent(rel_id: i64, pairs: &[(&str, &str)]) -> Parent {
        let fields = pairs.iter().map(|(k, v)| ((*k).to_owned().into(), (*v).to_owned().into())).collect();
        Parent { rel_id, record: Arc::new(ParentRecord { fields, timestamp: None, roles: Box::new([]) }), role: None }
    }

    #[test]
    fn merge_lets_the_later_parent_win_on_collision() {
        let merged = merge_parents(&[parent(1, &[("ref", "6"), ("route", "tram")]), parent(2, &[("ref", "8")])]);
        assert_eq!(merged.get("ref").map(|c| c.as_ref()), Some("8"));
        assert_eq!(merged.get("route").map(|c| c.as_ref()), Some("tram"));
    }
}
