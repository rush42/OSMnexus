//! Compact storage for a way's node-id list — `SelectionContext::way_refs`'s value type.
//!
//! A way's `node_refs` used to live as a resident `Vec<i64>` (8 bytes/id) per kept way, for the
//! whole run — on a country-sized extract with tens of millions of ways this rivals or exceeds the
//! node coordinate table in size, yet was untouched by `--disk-node-store` (that only spills
//! `node_coords`, see `disk_coords`'s own doc). Consecutive node ids along a way tend to be close
//! together (the PBF's own `DenseNodes` id encoding banks on the same locality), so delta+zigzag+
//! varint encoding shrinks most real-world ways well below 8 bytes/id, at the cost of a decode pass
//! (linear in the way's length) on every read instead of a slice index.
//!
//! This doesn't reach for the PBF's own on-wire delta encoding directly — `osmpbf`'s `Way::refs()`
//! already fully decodes it into absolute ids before we ever see them (see `resolve::way_data`), so
//! there's no compact representation left to borrow; this re-derives one from the decoded ids
//! instead of trying to bypass `osmpbf`'s way-parsing to reach the original varint bytes.

use rustc_hash::{FxHashMap, FxHashSet};
use rayon::prelude::*;

use crate::osm::types::OsmWay;

use super::disk_way_refs::DiskWayRefs;
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

    /// The raw encoded bytes — for a caller writing them into its own storage (e.g.
    /// `disk_way_refs::DiskWayRefs::build`, which concatenates every way's bytes into one mmap'd
    /// arena) rather than reading through this type.
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
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
        refs_first_last(&self.0)
    }
}

/// Free-standing counterparts of `EncodedRefs`' methods, operating directly on an encoded byte
/// slice — for a caller reading bytes it doesn't own an `EncodedRefs` for, e.g. `DiskWayRefs`
/// reading straight out of its mmap'd arena with no copy.
pub fn iter_refs(buf: &[u8]) -> RefsIter<'_> {
    RefsIter { buf, pos: 0, prev: 0 }
}

pub fn decode_refs(buf: &[u8]) -> Vec<i64> {
    iter_refs(buf).collect()
}

pub fn refs_first_last(buf: &[u8]) -> Option<(i64, i64)> {
    let mut it = iter_refs(buf);
    let first = it.next()?;
    let last = it.last().unwrap_or(first);
    Some((first, last))
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

/// `SelectionContext::way_refs`'s storage: `Memory` (default, a resident `FxHashMap`) or `Disk`
/// (the `--disk-node-store` opt-in — same flag `node_coords` uses, see `disk_way_refs`'s own doc
/// for why one way store earns a place next to the coordinate store). `build` is the only entry
/// point — both Pass A readers (`sorted`/`fallback`) already produce a plain
/// `FxHashMap<i64, (EncodedRefs, u32)>` from their own parallel per-blob accumulation regardless of
/// backend choice, so the backend split happens once, after that map is fully merged, rather than
/// threading a builder through Pass A's accumulation itself.
///
/// Exposes purpose-built methods for `geom::materialize`'s access patterns (resolve + route every
/// mask-!=0 way, resolve one arbitrary way on demand, collect mask-!=0 endpoints) instead of a
/// generic iterator — `Memory`'s and `Disk`'s underlying iterators are different concrete types, and
/// erasing that behind `dyn ParallelIterator` isn't supported by rayon, so each method just
/// branches once and lets both arms produce the same concrete result type. Deliberately has no
/// "resolve every way and cache it" method — see `par_route_kept`'s own doc for why.
pub enum WayRefsStore {
    Memory(FxHashMap<i64, (EncodedRefs, u32)>),
    Disk(DiskWayRefs),
}

impl WayRefsStore {
    pub fn build(map: FxHashMap<i64, (EncodedRefs, u32)>, disk: bool) -> anyhow::Result<Self> {
        if disk {
            let records: Vec<(i64, EncodedRefs, u32)> =
                map.into_iter().map(|(id, (refs, mask))| (id, refs, mask)).collect();
            Ok(WayRefsStore::Disk(DiskWayRefs::build(records)?))
        } else {
            Ok(WayRefsStore::Memory(map))
        }
    }

    pub fn len(&self) -> usize {
        match self {
            WayRefsStore::Memory(m) => m.len(),
            WayRefsStore::Disk(d) => d.len(),
        }
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
        match self {
            WayRefsStore::Memory(m) => m
                .par_iter()
                .filter(|(_, (_, mask))| *mask != 0)
                .filter_map(|(&id, (refs, mask))| {
                    let refs = refs.decode();
                    resolve_geometry(id, &refs, node_coords, selected).map(|w| (id, *mask, w))
                })
                .for_each(|(id, mask, w)| f(id, mask, w)),
            WayRefsStore::Disk(d) => d
                .par_iter()
                .filter(|(_, _, mask)| *mask != 0)
                .filter_map(|(id, bytes, mask)| {
                    let refs = decode_refs(bytes);
                    resolve_geometry(id, &refs, node_coords, selected).map(|w| (id, mask, w))
                })
                .for_each(|(id, mask, w)| f(id, mask, w)),
        }
    }

    /// Resolve one way's geometry on demand — for the small subset of ways (relation members) that
    /// `par_route_kept` doesn't already cover, without paying to cache every way for their sake.
    pub fn resolve_one(&self, id: i64, node_coords: &NodeCoords, selected: &FxHashSet<i64>) -> Option<OsmWay> {
        match self {
            WayRefsStore::Memory(m) => {
                let (refs, _) = m.get(&id)?;
                resolve_geometry(id, &refs.decode(), node_coords, selected)
            }
            WayRefsStore::Disk(d) => {
                let (bytes, _) = d.get(id)?;
                resolve_geometry(id, &decode_refs(bytes), node_coords, selected)
            }
        }
    }

    /// Every mask-!=0 way's first + last node id — the graph vertex candidate set.
    pub fn endpoints(&self) -> FxHashSet<i64> {
        match self {
            WayRefsStore::Memory(m) => m
                .iter()
                .filter(|(_, (_, mask))| *mask != 0)
                .filter_map(|(_, (refs, _))| refs.first_last())
                .flat_map(|(first, last)| [first, last])
                .collect(),
            WayRefsStore::Disk(d) => d
                .iter()
                .filter(|(_, _, mask)| *mask != 0)
                .filter_map(|(_, bytes, _)| refs_first_last(bytes))
                .flat_map(|(first, last)| [first, last])
                .collect(),
        }
    }

}

fn zigzag_encode(n: i64) -> u64 {
    ((n << 1) ^ (n >> 63)) as u64
}

fn zigzag_decode(u: u64) -> i64 {
    ((u >> 1) as i64) ^ -((u & 1) as i64)
}

fn write_varint(buf: &mut Vec<u8>, mut v: u64) {
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
fn read_varint(buf: &[u8], pos: &mut usize) -> u64 {
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
