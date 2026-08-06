//! Generic MPHF-indexed storage backends shared by every id-keyed structure that grows with input
//! size (node coords, node ref-counts, way refs, relation member lists).
//!
//! Two shapes, matching the two payload kinds these structures actually have:
//!
//! - `MphfArray<V>` / `MphfFile<V>` — fixed-size payloads (node coords, ref-counts): a resident
//!   `Vec<(i64, V)>` or an `mmap`'d flat file of `id | V::encode()` records, indexed by an
//!   `Mphf<i64>`. `MphfArray` is the in-memory backend (no serialization, no temp file); `MphfFile`
//!   is the `--use-disk-store` backend.
//! - `ArenaBuilder`/`ArenaStore` — variable-length payloads (way node-refs, relation member lists):
//!   a fixed 24-byte `id | offset | len | mask` index record (also `Mphf`-indexed) pointing into a
//!   second `mmap`'d arena holding the raw bytes back to back. `ArenaBuilder` streams inserts
//!   straight to the arena file as they're produced, keeping only the small index tuple resident
//!   per record — the memory-bounded construction path `--use-disk-store` exists for.
//!
//! Both MPHF variants build from the exact realized key set (inherent to the technique — the MPHF
//! can't be built incrementally) and verify the queried id against the stored id on every lookup
//! (`try_hash`, not `hash`: it isn't collision-free for ids outside the original key set, an
//! expected case here — e.g. a way referencing a node that was never collected).

use std::fs::File;
use std::io::{BufWriter, Write};

use boomphf::Mphf;
use memmap2::Mmap;
use rayon::prelude::*;

/// A fixed-size payload that can be packed into/out of a byte buffer — everything `MphfFile` needs
/// to know about what it's storing. The record's id is handled by the store itself, not by `V`.
pub trait FixedPayload: Copy {
    const LEN: usize;
    fn encode(&self, buf: &mut [u8]);
    fn decode(buf: &[u8]) -> Self;
}

/// MPHF-indexed, resident array of `(id, V)` records — the in-memory backend for a fixed-payload
/// store. No byte (de)serialization: `V` lives in the array as-is.
pub struct MphfArray<V> {
    mphf: Mphf<i64>,
    data: Vec<(i64, V)>,
}

impl<V: Copy> MphfArray<V> {
    /// Consumes `records`, builds a minimal perfect hash over the ids, and writes each record into
    /// its hashed slot. `empty` fills any slot a bug left unwritten (never a real id, so it reads as
    /// "absent" via the same id-mismatch check `get` already does).
    pub fn build(records: Vec<(i64, V)>, empty: V) -> Self {
        let len = records.len();
        let ids: Vec<i64> = records.iter().map(|&(id, _)| id).collect();
        // gamma=1.7 is boomphf's recommended default: a fair space/build-time tradeoff.
        let mphf = Mphf::new(1.7, &ids);
        drop(ids);

        let mut data = vec![(i64::MIN, empty); len];
        for (id, v) in records {
            let idx = mphf.hash(&id) as usize;
            data[idx] = (id, v);
        }

        MphfArray { mphf, data }
    }

    pub fn get(&self, id: i64) -> Option<V> {
        let idx = self.mphf.try_hash(&id)? as usize;
        let &(rid, v) = self.data.get(idx)?;
        if rid != id {
            return None;
        }
        Some(v)
    }

    pub fn len(&self) -> usize {
        self.data.len()
    }

    pub fn iter(&self) -> impl Iterator<Item = (i64, V)> + '_ {
        self.data.iter().map(|&(id, v)| (id, v))
    }
}

impl FixedPayload for u32 {
    const LEN: usize = 4;

    fn encode(&self, buf: &mut [u8]) {
        buf.copy_from_slice(&self.to_le_bytes());
    }

    fn decode(buf: &[u8]) -> Self {
        u32::from_le_bytes(buf.try_into().unwrap())
    }
}

const ID_LEN: usize = 8;

/// MPHF-indexed, `mmap`'d flat file of `id | V::encode()` records — the `--use-disk-store` backend
/// for a fixed-payload store.
pub struct MphfFile<V> {
    mphf: Mphf<i64>,
    mmap: Mmap,
    len: usize,
    record_len: usize,
    _file: tempfile::NamedTempFile,
    _marker: std::marker::PhantomData<V>,
}

impl<V: FixedPayload> MphfFile<V> {
    /// Consumes `records`, builds a minimal perfect hash over the ids, writes hash-indexed records
    /// to a temp file, and `mmap`s it read-only.
    pub fn build(records: Vec<(i64, V)>) -> anyhow::Result<Self> {
        let record_len = ID_LEN + V::LEN;
        let len = records.len();
        let ids: Vec<i64> = records.iter().map(|&(id, _)| id).collect();
        let mphf = Mphf::new(1.7, &ids);
        drop(ids);

        let mut buf = vec![0u8; len * record_len];
        for (id, v) in &records {
            let idx = mphf.hash(id) as usize;
            let off = idx * record_len;
            buf[off..off + ID_LEN].copy_from_slice(&id.to_le_bytes());
            v.encode(&mut buf[off + ID_LEN..off + record_len]);
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
        Ok(MphfFile { mphf, mmap, len, record_len, _file: file, _marker: std::marker::PhantomData })
    }

    fn record_at(&self, idx: usize) -> (i64, V) {
        let off = idx * self.record_len;
        let rec = &self.mmap[off..off + self.record_len];
        let id = i64::from_le_bytes(rec[0..ID_LEN].try_into().unwrap());
        let v = V::decode(&rec[ID_LEN..]);
        (id, v)
    }

    pub fn get(&self, id: i64) -> Option<V> {
        let idx = self.mphf.try_hash(&id)? as usize;
        if idx >= self.len {
            return None;
        }
        let (rid, v) = self.record_at(idx);
        if rid != id {
            return None;
        }
        Some(v)
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn iter(&self) -> impl Iterator<Item = (i64, V)> + '_ {
        (0..self.len).map(move |i| self.record_at(i))
    }
}

const INDEX_RECORD_LEN: usize = 24;

/// Streaming builder for `ArenaStore`: `insert` writes each record's bytes straight to the arena
/// file as it's produced and keeps only a small fixed-size `(id, offset, len, mask)` tuple per
/// record resident in `meta` — never holds more than one boxed-bytes allocation (the caller's) at a
/// time, instead of a fully resident `Vec<(i64, Box<[u8]>, u32)>` for the whole file.
pub struct ArenaBuilder {
    arena_writer: BufWriter<File>,
    arena_pos: u64,
    meta: Vec<(i64, u64, u32, u32)>,
    arena_file: tempfile::NamedTempFile,
}

impl ArenaBuilder {
    pub fn new() -> anyhow::Result<Self> {
        let arena_file = tempfile::NamedTempFile::new()?;
        let arena_writer = BufWriter::new(arena_file.reopen()?);
        Ok(ArenaBuilder { arena_writer, arena_pos: 0, meta: Vec::new(), arena_file })
    }

    pub fn insert(&mut self, id: i64, bytes: &[u8], mask: u32) -> anyhow::Result<()> {
        self.arena_writer.write_all(bytes)?;
        self.meta.push((id, self.arena_pos, bytes.len() as u32, mask));
        self.arena_pos += bytes.len() as u64;
        Ok(())
    }

    /// Builds the MPHF over `meta`'s ids (the only thing held fully resident, at 24 bytes/record)
    /// and writes the offset/len index at each id's hashed slot.
    pub fn finish(mut self) -> anyhow::Result<ArenaStore> {
        self.arena_writer.flush()?;
        drop(self.arena_writer);

        let len = self.meta.len();
        let ids: Vec<i64> = self.meta.iter().map(|&(id, ..)| id).collect();
        let mphf = Mphf::new(1.7, &ids);
        drop(ids);

        let mut index_buf = vec![0u8; len * INDEX_RECORD_LEN];
        for &(id, off, arena_len, mask) in &self.meta {
            let idx = mphf.hash(&id) as usize;
            let rec_off = idx * INDEX_RECORD_LEN;
            index_buf[rec_off..rec_off + 8].copy_from_slice(&id.to_le_bytes());
            index_buf[rec_off + 8..rec_off + 16].copy_from_slice(&off.to_le_bytes());
            index_buf[rec_off + 16..rec_off + 20].copy_from_slice(&arena_len.to_le_bytes());
            index_buf[rec_off + 20..rec_off + 24].copy_from_slice(mask.to_le_bytes().as_slice());
        }
        drop(self.meta);

        let index_file = tempfile::NamedTempFile::new()?;
        {
            let mut w = BufWriter::new(index_file.reopen()?);
            w.write_all(&index_buf)?;
            w.flush()?;
        }
        drop(index_buf);

        let index = unsafe { Mmap::map(index_file.as_file())? };
        let arena = unsafe { Mmap::map(self.arena_file.as_file())? };
        Ok(ArenaStore { mphf, index, arena, len, _index_file: index_file, _arena_file: self.arena_file })
    }
}

/// MPHF-indexed index file + byte arena — the `--use-disk-store` backend for a variable-length
/// payload store. Byte-oriented: callers own their own encode/decode (e.g. `EncodedRefs`), this
/// type just moves bytes + a `u32` mask around.
pub struct ArenaStore {
    mphf: Mphf<i64>,
    index: Mmap,
    arena: Mmap,
    len: usize,
    _index_file: tempfile::NamedTempFile,
    _arena_file: tempfile::NamedTempFile,
}

impl ArenaStore {
    /// Consumes `records`, writes every record's bytes into one arena file (in input order —
    /// arena position carries no meaning beyond "wherever this record's bytes happen to sit"),
    /// builds an MPHF over the ids, and writes the offset/len index at each id's hashed slot.
    pub fn build(records: Vec<(i64, Box<[u8]>, u32)>) -> anyhow::Result<Self> {
        let mut builder = ArenaBuilder::new()?;
        for (id, bytes, mask) in &records {
            builder.insert(*id, bytes, *mask)?;
        }
        builder.finish()
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

    /// `None` for an id that was never in the build set — `try_hash`, not `hash`; the id check
    /// stays load-bearing even past a `Some` hash result.
    pub fn get(&self, id: i64) -> Option<(&[u8], u32)> {
        let idx = self.mphf.try_hash(&id)? as usize;
        if idx >= self.len {
            return None;
        }
        let (rid, off, arena_len, mask) = self.index_record_at(idx);
        if rid != id {
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

    #[derive(Copy, Clone, Debug, PartialEq)]
    struct Coord {
        lon: f32,
        lat: f32,
        shared: bool,
    }

    impl FixedPayload for Coord {
        const LEN: usize = 9;
        fn encode(&self, buf: &mut [u8]) {
            buf[0..4].copy_from_slice(&self.lon.to_le_bytes());
            buf[4..8].copy_from_slice(&self.lat.to_le_bytes());
            buf[8] = self.shared as u8;
        }
        fn decode(buf: &[u8]) -> Self {
            Coord {
                lon: f32::from_le_bytes(buf[0..4].try_into().unwrap()),
                lat: f32::from_le_bytes(buf[4..8].try_into().unwrap()),
                shared: buf[8] != 0,
            }
        }
    }

    #[test]
    fn mphf_array_round_trips_present_ids_and_rejects_absent_ones() {
        let records = vec![
            (10i64, Coord { lon: 1.0, lat: 2.0, shared: false }),
            (20, Coord { lon: 3.0, lat: 4.0, shared: true }),
            (7_000_000_000, Coord { lon: 7.0, lat: 8.0, shared: true }),
        ];
        let store = MphfArray::build(records, Coord { lon: 0.0, lat: 0.0, shared: false });

        assert_eq!(store.len(), 3);
        assert_eq!(store.get(10), Some(Coord { lon: 1.0, lat: 2.0, shared: false }));
        assert_eq!(store.get(7_000_000_000), Some(Coord { lon: 7.0, lat: 8.0, shared: true }));
        assert_eq!(store.get(11), None);
        assert_eq!(store.get(0), None);
    }

    #[test]
    fn mphf_file_round_trips_present_ids_and_rejects_absent_ones() {
        let records = vec![
            (10i64, Coord { lon: 1.0, lat: 2.0, shared: false }),
            (20, Coord { lon: 3.0, lat: 4.0, shared: true }),
            (7_000_000_000, Coord { lon: 7.0, lat: 8.0, shared: true }),
        ];
        let store = MphfFile::build(records).unwrap();

        assert_eq!(store.len(), 3);
        assert_eq!(store.get(10), Some(Coord { lon: 1.0, lat: 2.0, shared: false }));
        assert_eq!(store.get(7_000_000_000), Some(Coord { lon: 7.0, lat: 8.0, shared: true }));
        assert_eq!(store.get(11), None);
        assert_eq!(store.get(0), None);
    }

    #[test]
    fn arena_store_round_trips_present_ids_and_rejects_absent_ones() {
        let records: Vec<(i64, Box<[u8]>, u32)> = vec![
            (10, vec![1, 2, 3].into_boxed_slice(), 1),
            (20, vec![].into_boxed_slice(), 0),
            (7_000_000_000, vec![9, 9, 9].into_boxed_slice(), 2),
        ];
        let store = ArenaStore::build(records).unwrap();

        assert_eq!(store.len(), 3);
        let (bytes, mask) = store.get(10).unwrap();
        assert_eq!(bytes, &[1, 2, 3]);
        assert_eq!(mask, 1);
        assert_eq!(store.get(11), None);
    }
}
