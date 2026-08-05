//! Disk-backed alternative to the in-memory `NodeCoords` hashmap, opt in via `--disk-node-store`.
//!
//! The in-memory map (`FxHashMap<i64, (f32,f32,bool)>`) stays resident for the whole run — through
//! Pass B *and* the materialize phase, alongside `way_refs` and `resolved` (see `geom::materialize`'s
//! own doc) — which is the dominant memory cost on a big extract. This trades that permanent
//! residency for a one-time minimal-perfect-hash build plus a flat, hash-indexed record file that's
//! `mmap`'d read-only, so the OS pages it in/out under memory pressure instead of it being pinned RSS.
//!
//! Lookup is O(1) (one MPHF hash + one mmap read) rather than binary search. The MPHF
//! (`boomphf::Mphf`, built from the exact key set and kept resident — a few bits/key) maps
//! `id -> index` via `try_hash`, not `hash`: `hash` assumes membership and panics if `id` was never
//! in the collected set (e.g. a way referencing a dropped/missing node), which is an expected,
//! frequent case here, not a bug. Even `try_hash`'s `Some` case is only guaranteed collision-free
//! *within* the key set — an absent id can still hash into another id's slot. The record therefore
//! still carries the full
//! id, used to verify the hashed slot actually belongs to the queried id (load-bearing for
//! correctness — e.g. a way referencing a node that was never collected must resolve to `None`, not
//! a neighbor's coordinates) and to hand back real ids from `iter()` (`assign_node_ids` in
//! `super::mod` matches these against `endpoints`/`selected` sets and stores them as `nodes` table
//! rows, so they must be the true OSM ids, not a truncated proxy).
//!
//! Record layout (17 bytes, unaligned — read via `from_le_bytes` on copied arrays, not a cast, so
//! there's no alignment requirement on the mmap): `id: i64 LE | lon: f32 LE | lat: f32 LE | shared:
//! u8`.

use std::io::{BufWriter, Write};

use boomphf::Mphf;
use memmap2::Mmap;

const RECORD_LEN: usize = 17;

/// MPHF-indexed, `mmap`'d flat file of node coordinate records, built once from every collected
/// `(id, lon, lat, shared)` tuple.
pub struct DiskNodeCoords {
    mphf: Mphf<i64>,
    mmap: Mmap,
    len: usize,
    // Kept alive for the lifetime of `mmap` — the temp file is unlinked on drop.
    _file: tempfile::NamedTempFile,
}

impl DiskNodeCoords {
    /// Consumes `records`, builds a minimal perfect hash over the ids, writes hash-indexed records
    /// to a temp file, and `mmap`s it read-only.
    pub fn build(records: Vec<(i64, f32, f32, bool)>) -> anyhow::Result<Self> {
        let len = records.len();
        let ids: Vec<i64> = records.iter().map(|&(id, ..)| id).collect();
        // gamma=1.7 is boomphf's recommended default: a fair space/build-time tradeoff.
        let mphf = Mphf::new(1.7, &ids);
        drop(ids);

        let mut buf = vec![0u8; len * RECORD_LEN];
        for (id, lon, lat, shared) in &records {
            let idx = mphf.hash(id) as usize;
            let off = idx * RECORD_LEN;
            buf[off..off + 8].copy_from_slice(&id.to_le_bytes());
            buf[off + 8..off + 12].copy_from_slice(&lon.to_le_bytes());
            buf[off + 12..off + 16].copy_from_slice(&lat.to_le_bytes());
            buf[off + 16] = *shared as u8;
        }
        drop(records);

        let file = tempfile::NamedTempFile::new()?;
        {
            let mut w = BufWriter::new(file.reopen()?);
            w.write_all(&buf)?;
            w.flush()?;
        }
        drop(buf);

        let mmap = unsafe { Mmap::map(file.as_file())? };
        Ok(DiskNodeCoords { mphf, mmap, len, _file: file })
    }

    fn record_at(&self, idx: usize) -> (i64, f32, f32, bool) {
        let off = idx * RECORD_LEN;
        let rec = &self.mmap[off..off + RECORD_LEN];
        let id = i64::from_le_bytes(rec[0..8].try_into().unwrap());
        let lon = f32::from_le_bytes(rec[8..12].try_into().unwrap());
        let lat = f32::from_le_bytes(rec[12..16].try_into().unwrap());
        let shared = rec[16] != 0;
        (id, lon, lat, shared)
    }

    pub fn get(&self, id: i64) -> Option<(f32, f32, bool)> {
        // `try_hash` (not `hash`) because `id` may never have been in the collected set (e.g. a way
        // referencing a node whose coords weren't kept) — `hash` panics in that case, since it
        // assumes membership; `try_hash` returns `None` once it exhausts all levels without a match.
        // Even the `Some` case is only "arbitrary but in range" for a truly absent key, so the id
        // check below stays load-bearing regardless.
        let idx = self.mphf.try_hash(&id)? as usize;
        if idx >= self.len {
            return None;
        }
        let (rid, lon, lat, shared) = self.record_at(idx);
        if rid != id {
            return None;
        }
        Some((lon, lat, shared))
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn iter(&self) -> impl Iterator<Item = (i64, (f32, f32, bool))> + '_ {
        (0..self.len).map(move |i| {
            let (id, lon, lat, shared) = self.record_at(i);
            (id, (lon, lat, shared))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_present_ids_and_rejects_absent_ones() {
        let records = vec![
            (10i64, 1.0f32, 2.0f32, false),
            (20, 3.0, 4.0, true),
            (30, 5.0, 6.0, false),
            (7_000_000_000, 7.0, 8.0, true), // exceeds u32, must not get truncated
        ];
        let store = DiskNodeCoords::build(records).unwrap();

        assert_eq!(store.len(), 4);
        assert_eq!(store.get(10), Some((1.0, 2.0, false)));
        assert_eq!(store.get(20), Some((3.0, 4.0, true)));
        assert_eq!(store.get(30), Some((5.0, 6.0, false)));
        assert_eq!(store.get(7_000_000_000), Some((7.0, 8.0, true)));

        // Ids never inserted must resolve to None even if they'd hash into an occupied slot.
        assert_eq!(store.get(11), None);
        assert_eq!(store.get(0), None);
        assert_eq!(store.get(-5), None);

        let mut collected: Vec<_> = store.iter().collect();
        collected.sort_by_key(|&(id, _)| id);
        assert_eq!(
            collected,
            vec![
                (10, (1.0, 2.0, false)),
                (20, (3.0, 4.0, true)),
                (30, (5.0, 6.0, false)),
                (7_000_000_000, (7.0, 8.0, true)),
            ]
        );
    }
}
