//! Compact storage for a way's node-id list — `SelectionContext::way_refs`'s value type.
//!
//! A way's `node_refs` used to live as a resident `Vec<i64>` (8 bytes/id) per kept way, for the
//! whole run — on a country-sized extract with tens of millions of ways this rivals or exceeds the
//! node coordinate table in size. Consecutive node ids along a way tend to be close together (the
//! PBF's own `DenseNodes` id encoding banks on the same locality), so delta+zigzag+varint encoding
//! shrinks most real-world ways well below 8 bytes/id, at the cost of a decode pass (linear in the
//! way's length) on every read instead of a slice index.
//!
//! This doesn't reach for the PBF's own on-wire delta encoding directly — `osmpbf`'s `Way::refs()`
//! already fully decodes it into absolute ids before we ever see them (see `resolve::way_data`), so
//! there's no compact representation left to borrow; this re-derives one from the decoded ids
//! instead of trying to bypass `osmpbf`'s way-parsing to reach the original varint bytes.

use rustc_hash::{FxHashMap, FxHashSet};
use rayon::prelude::*;

use crate::osm::types::OsmWay;

use super::resolve::{resolve_geometry, NodeCoords};

/// Delta+zigzag+varint encoded node-id list. `EncodedRefs::encode`/`iter` are the only way in/out —
/// callers never see the byte layout.
pub struct EncodedRefs(Box<[u8]>);

impl EncodedRefs {
    pub fn encode(ids: &[i64]) -> Self {
        let mut buf = Vec::with_capacity(ids.len() * 2);
        let mut prev = 0i64;
        for &id in ids {
            // Wrapping, not checked: real OSM ids never come close to i64's edges, but the
            // zigzag/varint round trip is well-defined under two's-complement wraparound regardless
            // (verified by the `i64::MIN`/`i64::MAX` case in this module's own tests), so there's no
            // need to make this fallible over an input shape that can't occur in practice.
            write_varint(&mut buf, zigzag_encode(id.wrapping_sub(prev)));
            prev = id;
        }
        EncodedRefs(buf.into_boxed_slice())
    }

    /// Same encoding as `encode`, but takes deltas that are already computed — `osmpbf`'s
    /// `Way::raw_refs()` hands back exactly this (the way's node refs as parsed off the wire,
    /// before `Way::refs()`'s iterator sums them into absolute ids), so a caller with access to that
    /// can skip both `Way::refs()`'s summation *and* `encode`'s re-subtraction, which are inverses
    /// of each other and cancel out to redundant work when chained. Deltas are zigzag-encoded
    /// as-is, no accumulation.
    pub fn from_deltas(deltas: &[i64]) -> Self {
        let mut buf = Vec::with_capacity(deltas.len() * 2);
        for &delta in deltas {
            write_varint(&mut buf, zigzag_encode(delta));
        }
        EncodedRefs(buf.into_boxed_slice())
    }

    pub fn iter(&self) -> RefsIter<'_> {
        iter_refs(&self.0)
    }

    pub fn decode(&self) -> Vec<i64> {
        self.iter().collect()
    }

    /// The way's first and last node id — its graph endpoints. Reading both still requires a full
    /// walk (the last id is a running sum of every delta), so this is no cheaper than `decode()`
    /// asymptotically, but it skips the `Vec` allocation callers that only need endpoints (not the
    /// full geometry) don't want to pay for.
    pub fn first_last(&self) -> Option<(i64, i64)> {
        let mut it = self.iter();
        let first = it.next()?;
        let last = it.last().unwrap_or(first);
        Some((first, last))
    }
}

fn iter_refs(buf: &[u8]) -> RefsIter<'_> {
    RefsIter { buf, pos: 0, prev: 0 }
}

pub struct RefsIter<'a> {
    buf: &'a [u8],
    pos: usize,
    prev: i64,
}

impl Iterator for RefsIter<'_> {
    type Item = i64;

    fn next(&mut self) -> Option<i64> {
        if self.pos >= self.buf.len() {
            return None;
        }
        let delta = zigzag_decode(read_varint(self.buf, &mut self.pos));
        let id = self.prev.wrapping_add(delta);
        self.prev = id;
        Some(id)
    }
}

/// `SelectionContext::way_refs`'s storage — a resident `FxHashMap<i64, (EncodedRefs, u32)>`, wrapped
/// so `geom::materialize`'s access patterns (resolve + route every mask-!=0 way, resolve one
/// arbitrary way on demand, collect mask-!=0 endpoints) get purpose-built methods instead of reaching
/// into the map directly. Deliberately has no "resolve every way and cache it" method — see
/// `par_route_kept`'s own doc for why.
pub struct WayRefsStore(FxHashMap<i64, (EncodedRefs, u32)>);

impl WayRefsStore {
    pub fn build(map: FxHashMap<i64, (EncodedRefs, u32)>) -> Self {
        WayRefsStore(map)
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Resolve and route every mask-!=0 way's geometry, in parallel — resolved once, handed straight
    /// to `f`, then dropped; no `resolved`-style cache of every kept way's geometry stays resident.
    /// A way that's *also* a relation member gets resolved a second time by `resolve_one` when
    /// relation-geometry assembly needs it — cheaper to redo than to keep the (typically much
    /// larger, since most kept ways aren't relation members — see `geom::materialize::run`'s own
    /// comment) full resolved set alive for that overlap's sake.
    pub fn par_route_kept<F>(&self, node_coords: &NodeCoords, selected: &FxHashSet<i64>, f: F)
    where
        F: Fn(i64, u32, OsmWay) + Sync,
    {
        self.0
            .par_iter()
            .filter(|(_, (_, mask))| *mask != 0)
            .filter_map(|(&id, (refs, mask))| {
                let refs = refs.decode();
                resolve_geometry(id, &refs, node_coords, selected).map(|w| (id, *mask, w))
            })
            .for_each(|(id, mask, w)| f(id, mask, w));
    }

    /// Resolve one way's geometry on demand — for the small subset of ways (relation members) that
    /// `par_route_kept` doesn't already cover, without paying to cache every way for their sake.
    pub fn resolve_one(&self, id: i64, node_coords: &NodeCoords, selected: &FxHashSet<i64>) -> Option<OsmWay> {
        let (refs, _) = self.0.get(&id)?;
        resolve_geometry(id, &refs.decode(), node_coords, selected)
    }

    /// Every mask-!=0 way's first + last node id — the graph vertex candidate set.
    pub fn endpoints(&self) -> FxHashSet<i64> {
        self.0
            .iter()
            .filter(|(_, (_, mask))| *mask != 0)
            .filter_map(|(_, (refs, _))| refs.first_last())
            .flat_map(|(first, last)| [first, last])
            .collect()
    }
}

pub(super) fn zigzag_encode(n: i64) -> u64 {
    ((n << 1) ^ (n >> 63)) as u64
}

pub(super) fn zigzag_decode(u: u64) -> i64 {
    ((u >> 1) as i64) ^ -((u & 1) as i64)
}

pub(super) fn write_varint(buf: &mut Vec<u8>, mut v: u64) {
    loop {
        let byte = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            buf.push(byte);
            return;
        }
        buf.push(byte | 0x80);
    }
}

/// Reads one varint starting at `*pos`, advancing it past the consumed bytes. Panics on a
/// truncated/malformed buffer — `EncodedRefs` is only ever built by `encode`, so a well-formed
/// buffer is an invariant, not an input to validate.
pub(super) fn read_varint(buf: &[u8], pos: &mut usize) -> u64 {
    let mut result = 0u64;
    let mut shift = 0u32;
    loop {
        let byte = buf[*pos];
        *pos += 1;
        result |= ((byte & 0x7f) as u64) << shift;
        if byte & 0x80 == 0 {
            return result;
        }
        shift += 7;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_ids() {
        let ids: Vec<i64> = vec![100, 101, 102, 50, 50, 7_000_000_000, -3, 0, i64::MAX, i64::MIN];
        let encoded = EncodedRefs::encode(&ids);
        assert_eq!(encoded.decode(), ids);
    }

    #[test]
    fn from_deltas_matches_encode() {
        let ids: Vec<i64> = vec![100, 101, 102, 50, 50, 7_000_000_000, -3, 0];
        let mut deltas = Vec::with_capacity(ids.len());
        let mut prev = 0i64;
        for &id in &ids {
            deltas.push(id - prev);
            prev = id;
        }
        assert_eq!(EncodedRefs::from_deltas(&deltas).decode(), ids);
        assert_eq!(EncodedRefs::from_deltas(&deltas).decode(), EncodedRefs::encode(&ids).decode());
    }

    #[test]
    fn first_last_matches_slice_semantics() {
        assert_eq!(EncodedRefs::encode(&[]).first_last(), None);
        assert_eq!(EncodedRefs::encode(&[42]).first_last(), Some((42, 42)));
        assert_eq!(EncodedRefs::encode(&[1, 2, 3]).first_last(), Some((1, 3)));
    }

    #[test]
    fn empty_list_round_trips() {
        let encoded = EncodedRefs::encode(&[]);
        assert_eq!(encoded.decode(), Vec::<i64>::new());
    }
}
