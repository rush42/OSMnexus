//! In-memory node coordinate store — the default `NodeCoords` backend, a thin wrapper over
//! `store::MphfArray`. Mirrors `disk_coords`'s MPHF-indexing strategy but keeps the record array
//! resident (`Vec`) instead of `mmap`'d, so there's no temp file or page faults, just a flat array
//! lookup.
//!
//! Replaces the previous plain `FxHashMap<i64, (f32,f32,bool)>`: a hashmap has to reserve slack for
//! its load factor and store the full key in every bucket regardless, and (worse for peak RSS at
//! this cardinality) grows by doubling, so the final `insert` can transiently hold both the old and
//! new bucket arrays live at once — the single largest contributor to Pass B's peak RSS before the
//! capacity-hint fix noted in `sorted::collect_coords`. Building the MPHF from the exact realized
//! key set and writing directly into a precisely-sized flat array sidesteps both: no load-factor
//! slack, and no growth-transient since the array is allocated once at its final size.
//!
//! See `disk_coords`'s own doc for why lookups still need the full id per record (not just the MPHF
//! index) — same reasoning applies here: `try_hash` isn't collision-free for ids outside the
//! original key set, and `iter()`'s ids feed `assign_node_ids`, which needs the true OSM id.

use super::store::MphfArray;

/// MPHF-indexed, resident array of node coordinate records, built once from every collected
/// `(id, lon, lat, shared)` tuple.
pub struct MemoryNodeCoords(MphfArray<(f32, f32, bool)>);

impl MemoryNodeCoords {
    pub fn build(records: Vec<(i64, f32, f32, bool)>) -> Self {
        let records = records.into_iter().map(|(id, lon, lat, shared)| (id, (lon, lat, shared))).collect();
        MemoryNodeCoords(MphfArray::build(records, (0.0, 0.0, false)))
    }

    pub fn get(&self, id: i64) -> Option<(f32, f32, bool)> {
        self.0.get(id)
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn iter(&self) -> impl Iterator<Item = (i64, (f32, f32, bool))> + '_ {
        self.0.iter()
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
            (7_000_000_000, 7.0, 8.0, true),
        ];
        let store = MemoryNodeCoords::build(records);

        assert_eq!(store.len(), 4);
        assert_eq!(store.get(10), Some((1.0, 2.0, false)));
        assert_eq!(store.get(20), Some((3.0, 4.0, true)));
        assert_eq!(store.get(30), Some((5.0, 6.0, false)));
        assert_eq!(store.get(7_000_000_000), Some((7.0, 8.0, true)));

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
