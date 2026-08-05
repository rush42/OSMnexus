//! Disk-backed alternative to the resident `FxHashMap<i64, (EncodedRefs, u32)>` way_refs store,
//! spilled under the same `--disk-node-store` flag as `disk_coords` (one flag, since both exist for
//! the same reason: bound the select-phase working set's contribution to peak RSS on a
//! country-sized extract — see `way_refs`'s own doc for why `way_refs` earns a place next to
//! `node_coords` here).
//!
//! Two `mmap`'d files, mirroring `disk_coords`'s MPHF-indexed-record approach but split in two
//! because a way's encoded refs are variable-length (unlike a fixed 17-byte coord record): a fixed
//! 24-byte index record per way (`id | arena offset | arena len | mask`), found via a minimal
//! perfect hash exactly as `disk_coords` does, pointing into a second file — the arena — holding
//! every way's `EncodedRefs` bytes concatenated back to back. A lookup is one MPHF hash, one index
//! read, and one arena slice — no second hash or search needed for the arena itself.

use std::io::{BufWriter, Write};

use boomphf::Mphf;
use memmap2::Mmap;
use rayon::prelude::*;

use super::way_refs::EncodedRefs;

const INDEX_RECORD_LEN: usize = 24;

pub struct DiskWayRefs {
    mphf: Mphf<i64>,
    index: Mmap,
    arena: Mmap,
    len: usize,
    _index_file: tempfile::NamedTempFile,
    _arena_file: tempfile::NamedTempFile,
}

impl DiskWayRefs {
    /// Consumes `records`, writes every way's encoded refs into one arena file (in input order —
    /// arena position carries no meaning beyond "wherever this way's bytes happen to sit"), builds
    /// an MPHF over the way ids, and writes the offset/len index at each way's hashed slot.
    pub fn build(records: Vec<(i64, EncodedRefs, u32)>) -> anyhow::Result<Self> {
        let len = records.len();
        let ids: Vec<i64> = records.iter().map(|&(id, ..)| id).collect();
        // gamma=1.7 is boomphf's recommended default: a fair space/build-time tradeoff.
        let mphf = Mphf::new(1.7, &ids);
        drop(ids);

        let arena_file = tempfile::NamedTempFile::new()?;
        let mut arena_pos: Vec<(u64, u32)> = Vec::with_capacity(len);
        {
            let mut w = BufWriter::new(arena_file.reopen()?);
            let mut pos: u64 = 0;
            for (_, refs, _) in &records {
                let bytes = refs.as_bytes();
                w.write_all(bytes)?;
                arena_pos.push((pos, bytes.len() as u32));
                pos += bytes.len() as u64;
            }
            w.flush()?;
        }

        let mut index_buf = vec![0u8; len * INDEX_RECORD_LEN];
        for (i, (id, _, mask)) in records.iter().enumerate() {
            let idx = mphf.hash(id) as usize;
            let off = idx * INDEX_RECORD_LEN;
            let (arena_off, arena_len) = arena_pos[i];
            index_buf[off..off + 8].copy_from_slice(&id.to_le_bytes());
            index_buf[off + 8..off + 16].copy_from_slice(&arena_off.to_le_bytes());
            index_buf[off + 16..off + 20].copy_from_slice(&arena_len.to_le_bytes());
            index_buf[off + 20..off + 24].copy_from_slice(mask.to_le_bytes().as_slice());
        }
        drop(records);
        drop(arena_pos);

        let index_file = tempfile::NamedTempFile::new()?;
        {
            let mut w = BufWriter::new(index_file.reopen()?);
            w.write_all(&index_buf)?;
            w.flush()?;
        }
        drop(index_buf);

        let index = unsafe { Mmap::map(index_file.as_file())? };
        let arena = unsafe { Mmap::map(arena_file.as_file())? };
        Ok(DiskWayRefs { mphf, index, arena, len, _index_file: index_file, _arena_file: arena_file })
    }

    fn index_record_at(&self, i: usize) -> (i64, u64, u32, u32) {
        let off = i * INDEX_RECORD_LEN;
        let rec = &self.index[off..off + INDEX_RECORD_LEN];
        let id = i64::from_le_bytes(rec[0..8].try_into().unwrap());
        let arena_off = u64::from_le_bytes(rec[8..16].try_into().unwrap());
        let arena_len = u32::from_le_bytes(rec[16..20].try_into().unwrap());
        let mask = u32::from_le_bytes(rec[20..24].try_into().unwrap());
        (id, arena_off, arena_len, mask)
    }

    fn arena_slice(&self, off: u64, len: u32) -> &[u8] {
        let off = off as usize;
        &self.arena[off..off + len as usize]
    }

    /// `None` for a way id that was never in the build set — same reasoning as
    /// `disk_coords::DiskNodeCoords::get` (`try_hash`, not `hash`; the id check stays load-bearing
    /// even past a `Some` hash result).
    pub fn get(&self, way_id: i64) -> Option<(&[u8], u32)> {
        let idx = self.mphf.try_hash(&way_id)? as usize;
        if idx >= self.len {
            return None;
        }
        let (rid, off, arena_len, mask) = self.index_record_at(idx);
        if rid != way_id {
            return None;
        }
        Some((self.arena_slice(off, arena_len), mask))
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn iter(&self) -> impl Iterator<Item = (i64, &[u8], u32)> + '_ {
        (0..self.len).map(move |i| {
            let (id, off, arena_len, mask) = self.index_record_at(i);
            (id, self.arena_slice(off, arena_len), mask)
        })
    }

    pub fn par_iter(&self) -> impl ParallelIterator<Item = (i64, &[u8], u32)> + '_ {
        (0..self.len).into_par_iter().map(move |i| {
            let (id, off, arena_len, mask) = self.index_record_at(i);
            (id, self.arena_slice(off, arena_len), mask)
        })
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
