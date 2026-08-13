//! `SelectionContext::rel_members`'s storage — the relation analog of `way_refs`: a relation's
//! member-way list (id + role) is exactly as variable-length as a way's node-ref list, and grows the
//! same way with input size (kept relations × members), so it shares the same [`MphfArena`] backend
//! `way_refs` sits on (see `store`'s own doc for the shared mechanics).
//!
//! `EncodedMembers` mirrors `EncodedRefs`' delta+zigzag+varint id encoding, with one byte per member
//! appended for its `MemberRole` — relation member counts are small enough (a country-sized extract
//! has orders of magnitude fewer relations than ways) that packing the role into spare varint bits
//! isn't worth the complexity `EncodedRefs` avoids for way refs either.

use rustc_hash::{FxHashMap, FxHashSet};

use crate::osm::types::MemberRole;

use super::store::MphfArena;
use super::way_refs::{read_varint, write_varint, zigzag_decode, zigzag_encode};

/// Delta+zigzag+varint encoded `(way_id, role)` list. `encode`/`decode_members` are the only way
/// in/out — callers never see the byte layout.
pub struct EncodedMembers(Box<[u8]>);

impl EncodedMembers {
    pub fn encode(members: &[(i64, MemberRole)]) -> Self {
        let mut buf = Vec::with_capacity(members.len() * 3);
        let mut prev = 0i64;
        for &(id, role) in members {
            write_varint(&mut buf, zigzag_encode(id.wrapping_sub(prev)));
            buf.push(role.as_u8());
            prev = id;
        }
        EncodedMembers(buf.into_boxed_slice())
    }
}

pub fn decode_members(buf: &[u8]) -> Vec<(i64, MemberRole)> {
    let mut out = Vec::new();
    let mut pos = 0;
    let mut prev = 0i64;
    while pos < buf.len() {
        let delta = zigzag_decode(read_varint(buf, &mut pos));
        let id = prev.wrapping_add(delta);
        prev = id;
        let role = MemberRole::from_u8(buf[pos]);
        pos += 1;
        out.push((id, role));
    }
    out
}

/// `SelectionContext::rel_members`'s storage: an MPHF-indexed arena of `relation_id -> (member ways
/// with role, keep mask)`. Exposes the two access patterns `geom::materialize` actually needs
/// (every member way id across all relations, and the full per-relation request list) instead of a
/// generic iterator — same reasoning as `WayRefsStore`'s own purpose-built methods.
pub struct RelMembers(MphfArena<u32>);

impl RelMembers {
    pub fn build(map: FxHashMap<i64, (Vec<(i64, MemberRole)>, u32)>) -> Self {
        let records: Vec<(i64, Box<[u8]>, u32)> =
            map.into_iter().map(|(id, (members, mask))| (id, EncodedMembers::encode(&members).0, mask)).collect();
        RelMembers(MphfArena::build(records))
    }

    pub fn is_empty(&self) -> bool {
        self.0.len() == 0
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Every member way id across every relation, deduplicated.
    pub fn member_way_ids(&self) -> FxHashSet<i64> {
        self.0.iter().flat_map(|(_, bytes, _)| decode_members(bytes).into_iter().map(|(w, _)| w)).collect()
    }

    /// `(relation_id, member ways with role, keep mask)` for every relation in `order` —
    /// `geom::materialize`'s relation-geometry assembly input. Takes an explicit order (rather than
    /// walking the arena's own MPHF-slot order, an id-derived order unrelated to when relations were
    /// classified) so the caller can pass `SelectionContext::kept_relation_order` — the same blob
    /// order the relations pass already routed each relation's tag row in — and get relation
    /// geometry rows out in matching order, same reasoning as `WayRefsStore::par_route_ordered`.
    /// Sequential, not chunked/parallel like the way equivalent: relation counts are orders of
    /// magnitude smaller than way counts (see this module's own doc), so there's no transient-size
    /// concern worth the complexity.
    pub fn requests_ordered(&self, order: &[i64]) -> Vec<(i64, Vec<(i64, MemberRole)>, u32)> {
        order
            .iter()
            .filter_map(|&id| {
                let (bytes, mask) = self.0.get(id)?;
                Some((id, decode_members(bytes), mask))
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encoded_members_round_trips() {
        let members = vec![(100, MemberRole::Outer), (50, MemberRole::Inner), (7_000_000_000, MemberRole::Unknown)];
        let encoded = EncodedMembers::encode(&members);
        assert_eq!(decode_members(&encoded.0), members);
    }

    #[test]
    fn rel_members_round_trips_present_ids_and_rejects_absent_ones() {
        let mut map: FxHashMap<i64, (Vec<(i64, MemberRole)>, u32)> = FxHashMap::default();
        map.insert(1, (vec![(10, MemberRole::Outer), (20, MemberRole::Inner)], 3));
        map.insert(2, (vec![(30, MemberRole::Unknown)], 1));

        let store = RelMembers::build(map);
        assert_eq!(store.len(), 2);
        assert!(!store.is_empty());

        let mut way_ids: Vec<i64> = store.member_way_ids().into_iter().collect();
        way_ids.sort();
        assert_eq!(way_ids, vec![10, 20, 30]);

        let mut requests = store.requests_ordered(&[1, 2]);
        requests.sort_by_key(|&(id, ..)| id);
        assert_eq!(
            requests,
            vec![
                (1, vec![(10, MemberRole::Outer), (20, MemberRole::Inner)], 3),
                (2, vec![(30, MemberRole::Unknown)], 1),
            ]
        );
    }
}
