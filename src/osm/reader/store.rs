//! Shared MPHF-indexed arena store for `id -> (variable-length bytes, fixed-size meta)` maps —
//! `way_refs` (meta = keep mask, bytes = a way's delta-varint node refs) and `rel_members` (meta =
//! keep mask, bytes = a relation's delta-varint member list) are the same shape: a map keyed by OSM
//! id, growing with input size, whose value is an encoded byte blob plus one `u32`. Both used to be
//! a plain `FxHashMap`, which reserves load-factor slack and stores the full key in every bucket —
//! the same waste `NodeCoords` (`memory_coords.rs`) moved off of by building a minimal perfect hash
//! over the exact realized key set instead. This generalizes that to a variable-length payload: one
//! flat byte arena plus an offsets index, both addressed by the same MPHF slot as the id/meta arrays.

use boomphf::Mphf;
use rayon::prelude::*;

pub struct MphfArena<T> {
    mphf: Mphf<i64>,
    ids: Box<[i64]>,
    offsets: Box<[u32]>, // len+1 entries; slot i's bytes are offsets[i]..offsets[i+1]
    bytes: Box<[u8]>,
    meta: Box<[T]>,
}

impl<T: Copy + Send + Sync> MphfArena<T> {
    /// Consumes `records`, builds a minimal perfect hash over the ids, and lays out each record's
    /// bytes into one arena ordered by hashed slot (so a slot's byte range is recoverable from just
    /// its neighbors' offsets, no per-record length field needed).
    pub fn build(records: Vec<(i64, Box<[u8]>, T)>) -> Self {
        let ids_for_mphf: Vec<i64> = records.iter().map(|(id, ..)| *id).collect();
        // gamma=1.7 is boomphf's recommended default: a fair space/build-time tradeoff — same
        // choice `NodeCoords::build` makes.
        let mphf = Mphf::new(1.7, &ids_for_mphf);
        drop(ids_for_mphf);

        let mut slotted: Vec<(usize, i64, Box<[u8]>, T)> =
            records.into_iter().map(|(id, bytes, meta)| (mphf.hash(&id) as usize, id, bytes, meta)).collect();
        slotted.sort_unstable_by_key(|(idx, ..)| *idx);

        let len = slotted.len();
        let mut ids = Vec::with_capacity(len);
        let mut meta = Vec::with_capacity(len);
        let mut offsets = Vec::with_capacity(len + 1);
        let mut bytes = Vec::new();
        offsets.push(0u32);
        for (_, id, b, m) in slotted {
            ids.push(id);
            meta.push(m);
            bytes.extend_from_slice(&b);
            offsets.push(bytes.len() as u32);
        }

        MphfArena {
            mphf,
            ids: ids.into_boxed_slice(),
            offsets: offsets.into_boxed_slice(),
            bytes: bytes.into_boxed_slice(),
            meta: meta.into_boxed_slice(),
        }
    }

    pub fn len(&self) -> usize {
        self.ids.len()
    }

    fn slot(&self, idx: usize) -> (i64, &[u8], T) {
        let start = self.offsets[idx] as usize;
        let end = self.offsets[idx + 1] as usize;
        (self.ids[idx], &self.bytes[start..end], self.meta[idx])
    }

    /// `try_hash` (not `hash`) because `id` may never have been in the build set — same reasoning
    /// as `NodeCoords::get`; the id check below is load-bearing for the same reason.
    pub fn get(&self, id: i64) -> Option<(&[u8], T)> {
        let idx = self.mphf.try_hash(&id)? as usize;
        let (rid, bytes, meta) = self.slot(idx);
        (rid == id).then_some((bytes, meta))
    }

    pub fn iter(&self) -> impl Iterator<Item = (i64, &[u8], T)> + '_ {
        (0..self.ids.len()).map(move |idx| self.slot(idx))
    }

    pub fn par_iter(&self) -> impl ParallelIterator<Item = (i64, &[u8], T)> + '_ {
        (0..self.ids.len()).into_par_iter().map(move |idx| self.slot(idx))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_present_ids_and_rejects_absent_ones() {
        let records: Vec<(i64, Box<[u8]>, u32)> = vec![
            (10, vec![1, 2, 3].into_boxed_slice(), 1),
            (20, vec![].into_boxed_slice(), 2),
            (7_000_000_000, vec![9, 9].into_boxed_slice(), 3),
        ];
        let arena = MphfArena::build(records);

        assert_eq!(arena.len(), 3);
        assert_eq!(arena.get(10), Some((&[1u8, 2, 3][..], 1)));
        assert_eq!(arena.get(20), Some((&[][..], 2)));
        assert_eq!(arena.get(7_000_000_000), Some((&[9u8, 9][..], 3)));
        assert_eq!(arena.get(11), None);
        assert_eq!(arena.get(0), None);

        let mut collected: Vec<_> = arena.iter().map(|(id, b, m)| (id, b.to_vec(), m)).collect();
        collected.sort_by_key(|&(id, ..)| id);
        assert_eq!(
            collected,
            vec![(10, vec![1, 2, 3], 1), (20, vec![], 2), (7_000_000_000, vec![9, 9], 3)]
        );
    }
}
