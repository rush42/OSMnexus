//! Disk-backed alternative to the resident `FxHashMap<i64, (EncodedRefs, u32)>` way_refs store,
//! spilled under the same `--use-disk-store` flag as `disk_coords` (one flag, since both exist for
//! the same reason: bound the select-phase working set's contribution to peak RSS on a
//! country-sized extract — see `way_refs`'s own doc for why `way_refs` earns a place next to
//! `node_coords` here).
//!
//! Thin wrappers over `store::ArenaBuilder`/`store::ArenaStore` — see that module's doc for the
//! shared MPHF/mmap/streaming-insert mechanics. `EncodedRefs` bytes are the arena payload; `mask` is
//! carried alongside as `ArenaStore` already supports generically.

use super::store::{ArenaBuilder, ArenaStore};
use super::way_refs::EncodedRefs;

/// Streaming counterpart to `DiskWayRefs::build`: instead of taking a fully-resident
/// `Vec<(i64, EncodedRefs, u32)>` (one separate heap allocation per way, for every way in the
/// file — exactly the memory shape `--use-disk-store` exists to avoid), `insert` writes each way's
/// encoded bytes straight to the arena file as it's produced. Meant to be fed from a
/// bounded-parallel-decode-then-sequential-fold loop, same shape as `sorted::collect_coords`'s
/// `FOLD_CHUNK_BLOBS` pattern — `insert` itself isn't `Sync`, callers serialize their own writes.
pub struct WayRefsBuilder(ArenaBuilder);

impl WayRefsBuilder {
    pub fn new() -> anyhow::Result<Self> {
        Ok(WayRefsBuilder(ArenaBuilder::new()?))
    }

    pub fn insert(&mut self, id: i64, refs: &EncodedRefs, mask: u32) -> anyhow::Result<()> {
        self.0.insert(id, refs.as_bytes(), mask)
    }

    pub fn finish(self) -> anyhow::Result<DiskWayRefs> {
        Ok(DiskWayRefs(self.0.finish()?))
    }
}

pub struct DiskWayRefs(ArenaStore);

impl DiskWayRefs {
    /// Consumes `records`, writes every way's encoded refs into one arena file, builds an MPHF over
    /// the way ids, and writes the offset/len index at each way's hashed slot.
    pub fn build(records: Vec<(i64, EncodedRefs, u32)>) -> anyhow::Result<Self> {
        let records =
            records.into_iter().map(|(id, refs, mask)| (id, refs.as_bytes().into(), mask)).collect();
        Ok(DiskWayRefs(ArenaStore::build(records)?))
    }

    pub fn get(&self, way_id: i64) -> Option<(&[u8], u32)> {
        self.0.get(way_id)
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn iter(&self) -> impl Iterator<Item = (i64, &[u8], u32)> + '_ {
        self.0.iter()
    }

    pub fn par_iter(&self) -> impl rayon::iter::ParallelIterator<Item = (i64, &[u8], u32)> + '_ {
        self.0.par_iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::way_refs::decode_refs;

    #[test]
    fn round_trips_present_ids_and_rejects_absent_ones() {
        let records = vec![
            (10i64, EncodedRefs::encode(&[1, 2, 3]), 1u32),
            (20, EncodedRefs::encode(&[100, 200, 150]), 0),
            (30, EncodedRefs::encode(&[]), 5),
            (7_000_000_000, EncodedRefs::encode(&[9, 9, 9]), 2),
        ];
        let store = DiskWayRefs::build(records).unwrap();

        assert_eq!(store.len(), 4);
        let (bytes, mask) = store.get(10).unwrap();
        assert_eq!(decode_refs(bytes), vec![1, 2, 3]);
        assert_eq!(mask, 1);

        let (bytes, mask) = store.get(20).unwrap();
        assert_eq!(decode_refs(bytes), vec![100, 200, 150]);
        assert_eq!(mask, 0);

        assert_eq!(store.get(11), None);
        assert_eq!(store.get(0), None);

        let mut collected: Vec<(i64, Vec<i64>, u32)> =
            store.iter().map(|(id, bytes, mask)| (id, decode_refs(bytes), mask)).collect();
        collected.sort_by_key(|&(id, _, _)| id);
        assert_eq!(
            collected,
            vec![
                (10, vec![1, 2, 3], 1),
                (20, vec![100, 200, 150], 0),
                (30, vec![], 5),
                (7_000_000_000, vec![9, 9, 9], 2),
            ]
        );
    }
}
