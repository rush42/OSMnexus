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
use super::store::MphfArena;

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

/// `SelectionContext::way_refs`'s storage — an MPHF-indexed [`MphfArena`] (see `store`'s own doc
/// for why this replaced a resident `FxHashMap<i64, (EncodedRefs, u32)>`), wrapped so
/// `geom::materialize`'s access patterns (resolve + route every mask-!=0 way, resolve one arbitrary
/// way on demand, collect mask-!=0 endpoints) get purpose-built methods instead of reaching into the
/// arena directly. Deliberately has no "resolve every way and cache it" method — see
/// `par_route_ordered`'s own doc for why.
pub struct WayRefsStore(MphfArena<u32>);

/// Floor — the value this was fixed at before it became thread-relative. Bounds the transient of
/// resolved-but-not-yet-routed geometry (coordinates, WKB bytes) held between a chunk's parallel
/// resolve and its sequential route; see `route_chunk_ways` for how the batch is actually sized.
const ROUTE_CHUNK_WAYS_MIN: usize = 512;

/// Ways resolved per parallel batch, sized against the actual rayon pool.
///
/// A fixed 512 is a *fork/join barrier* every 512 ways, and the barrier does not get cheaper as the
/// pool grows: at 56 threads it left ~9 ways of work per thread per barrier, so the synchronisation
/// cost swamped the work it guarded. Measured on `germany-latest.osm.pbf` (`configs/tilda`,
/// `--output parquet`), the materialize phase improved 4.3x from 1 to 16 threads (148.7s → 34.6s)
/// and then *regressed* to 60.3s at 56 — a phase getting slower on more threads for the same
/// workload, which is the signature of barrier overhead rather than of the work itself. See
/// `output_master_plan.md` §1.3/§2.
///
/// `PER_THREAD` keeps each worker's share of a batch roughly constant instead, so the barrier is
/// amortised the same way at 8 threads as at 56. Still bounded, and still for the original reason:
/// the transient here is a chunk's resolved-but-not-yet-routed geometry (coordinates, WKB bytes),
/// so this trades peak RSS for throughput and any change to `PER_THREAD` must be re-measured
/// against RSS, not just wall time — `fold_chunk_blobs` learned that the hard way.
fn route_chunk_ways() -> usize {
    const PER_THREAD: usize = 128;
    (rayon::current_num_threads() * PER_THREAD).max(ROUTE_CHUNK_WAYS_MIN)
}

impl WayRefsStore {
    pub fn build(map: FxHashMap<i64, (EncodedRefs, u32)>) -> Self {
        let records: Vec<(i64, Box<[u8]>, u32)> =
            map.into_iter().map(|(id, (refs, mask))| (id, refs.0, mask)).collect();
        WayRefsStore(MphfArena::build(records))
    }

    /// Same as [`build`](Self::build), from a flat `Vec` instead of a `FxHashMap` — for callers
    /// (`sorted::classify_and_index`) that never needed key lookups or dedup while accumulating,
    /// only the final MPHF-arena layout `build` produces either way (`MphfArena::build` re-sorts
    /// every record into hashed-slot order regardless of input order, so a `Vec`'s push-order input
    /// costs nothing extra here that a `FxHashMap`'s insert-order input wouldn't).
    pub fn build_records(records: Vec<(i64, EncodedRefs, u32)>) -> Self {
        let records: Vec<(i64, Box<[u8]>, u32)> = records.into_iter().map(|(id, refs, mask)| (id, refs.0, mask)).collect();
        WayRefsStore(MphfArena::build(records))
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Resolve and route every way id in `order`'s geometry, in `order`'s own relative order —
    /// bounded parallel chunks (`route_chunk_ways`), each chunk resolved in parallel then routed
    /// sequentially, relying on rayon's indexed `collect` preserving a chunk's input order. The
    /// caller (`geom::materialize::run`) passes the same blob order the select phase already routed
    /// each way's tag row in (`SelectionContext::kept_way_order`), so a table's tag-row CSV and its
    /// paired geometry CSV now end up in matching row order — no join needed to correlate them by
    /// `osm_id` downstream (previously this iterated the arena's own MPHF-slot order, an
    /// id-derived order unrelated to either file's tag-row order).
    ///
    /// Resolved once, handed straight to `f`, then dropped; no `resolved`-style cache of every kept
    /// way's geometry stays resident. A way that's *also* a relation member gets resolved a second
    /// time by `resolve_one` when relation-geometry assembly needs it — cheaper to redo than to keep
    /// the (typically much larger, since most kept ways aren't relation members — see
    /// `geom::materialize::run`'s own comment) full resolved set alive for that overlap's sake.
    pub fn par_route_ordered<F>(&self, order: &[i64], node_coords: &NodeCoords, selected: &FxHashSet<i64>, f: F)
    where
        F: Fn(i64, u32, OsmWay) + Sync,
    {
        for id_chunk in order.chunks(route_chunk_ways()) {
            let resolved: Vec<(i64, u32, OsmWay)> = id_chunk
                .par_iter()
                .filter_map(|&id| {
                    let (bytes, mask) = self.0.get(id)?;
                    let refs: Vec<i64> = iter_refs(bytes).collect();
                    resolve_geometry(id, &refs, node_coords, selected).map(|w| (id, mask, w))
                })
                .collect();
            for (id, mask, w) in resolved {
                f(id, mask, w);
            }
        }
    }

    /// Resolve and route every mask-!=0 way's geometry, in arena (MPHF-slot) order — the unordered
    /// counterpart to `par_route_ordered`, for output backends with no row-order correlation to
    /// preserve downstream (e.g. `pg`, joined on `osm_id`). Fully parallel: unlike
    /// `par_route_ordered`, there's no caller-given order to preserve, so no chunk-then-fold split is
    /// needed — `f` is called directly from whichever rayon worker resolved that way, same
    /// concurrency contract `par_route_ordered` already requires of it.
    pub fn par_route_all<F>(&self, node_coords: &NodeCoords, selected: &FxHashSet<i64>, f: F)
    where
        F: Fn(i64, u32, OsmWay) + Sync,
    {
        self.0.par_iter().for_each(|(id, bytes, mask)| {
            if mask == 0 {
                return;
            }
            let refs: Vec<i64> = iter_refs(bytes).collect();
            if let Some(w) = resolve_geometry(id, &refs, node_coords, selected) {
                f(id, mask, w);
            }
        });
    }

    /// Resolve one way's geometry on demand — for the small subset of ways (relation members) that
    /// `par_route_ordered` doesn't already cover, without paying to cache every way for their sake.
    /// See `MphfArena::heap_bytes` — a lower bound (excludes the MPHF).
    pub fn heap_bytes(&self) -> usize {
        self.0.heap_bytes()
    }

    pub fn resolve_one(&self, id: i64, node_coords: &NodeCoords, selected: &FxHashSet<i64>) -> Option<OsmWay> {
        let (bytes, _) = self.0.get(id)?;
        let refs: Vec<i64> = iter_refs(bytes).collect();
        resolve_geometry(id, &refs, node_coords, selected)
    }

    /// Every mask-!=0 way's first + last node id — the graph vertex candidate set.
    pub fn endpoints(&self) -> FxHashSet<i64> {
        self.0
            .iter()
            .filter(|(_, _, mask)| *mask != 0)
            .filter_map(|(_, bytes, _)| {
                let mut it = iter_refs(bytes);
                let first = it.next()?;
                let last = it.last().unwrap_or(first);
                Some((first, last))
            })
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
