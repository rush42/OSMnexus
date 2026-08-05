//! Disk-backed alternative to the in-memory `NodeCoords` hashmap, opt in via `--disk-node-store`.
//!
//! The in-memory map (`FxHashMap<i64, (f32,f32,bool)>`) stays resident for the whole run — through
//! Pass B *and* the materialize phase, alongside `way_refs` and `resolved` (see `geom::materialize`'s
//! own doc) — which is the dominant memory cost on a big extract. This trades that permanent
//! residency for a one-time sort + a flat, sorted-by-id record file that's `mmap`'d read-only and
//! looked up by binary search, so the OS pages it in/out under memory pressure instead of it being
//! pinned RSS.
//!
//! Record layout (17 bytes, unaligned — read via `from_le_bytes` on copied arrays, not a cast, so
//! there's no alignment requirement on the mmap): `id: i64 LE | lon: f32 LE | lat: f32 LE | shared: u8`.

use std::io::{BufWriter, Write};

use memmap2::Mmap;

const RECORD_LEN: usize = 17;

/// Sorted-by-id, `mmap`'d flat file of node coordinate records. Built once from every collected
/// `(id, lon, lat, shared)` tuple (which requires holding them in a `Vec` transiently — the sorted
/// reader path only ever produces them in ascending id order already, so no reorder is needed there
/// in practice, but the fallback path can't promise that, hence the explicit sort here for both).
pub struct DiskNodeCoords {
    mmap: Mmap,
    len: usize,
    // Kept alive for the lifetime of `mmap` — the temp file is unlinked on drop.
    _file: tempfile::NamedTempFile,
}

impl DiskNodeCoords {
    /// Consumes `records`, sorts them by id, writes them to a temp file, and `mmap`s it read-only.
    pub fn build(mut records: Vec<(i64, f32, f32, bool)>) -> anyhow::Result<Self> {
        records.sort_unstable_by_key(|&(id, ..)| id);
        let len = records.len();

        let file = tempfile::NamedTempFile::new()?;
        {
            let mut w = BufWriter::new(file.reopen()?);
            let mut buf = [0u8; RECORD_LEN];
            for (id, lon, lat, shared) in &records {
                buf[0..8].copy_from_slice(&id.to_le_bytes());
                buf[8..12].copy_from_slice(&lon.to_le_bytes());
                buf[12..16].copy_from_slice(&lat.to_le_bytes());
                buf[16] = *shared as u8;
                w.write_all(&buf)?;
            }
            w.flush()?;
        }
        drop(records);

        let mmap = unsafe { Mmap::map(file.as_file())? };
        Ok(DiskNodeCoords { mmap, len, _file: file })
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
        let mut lo = 0usize;
        let mut hi = self.len;
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let (rid, lon, lat, shared) = self.record_at(mid);
            match rid.cmp(&id) {
                std::cmp::Ordering::Equal => return Some((lon, lat, shared)),
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
            }
        }
        None
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
