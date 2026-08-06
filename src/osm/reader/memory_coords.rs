//! Node coordinate store — MPHF-indexed, resident array of `(id, lon, lat, shared)` records, built
//! once from every collected coordinate.
//!
//! Replaces a plain `FxHashMap<i64, (f32,f32,bool)>`: a hashmap has to reserve slack for its load
//! factor and store the full key in every bucket regardless, and (worse for peak RSS at this
//! cardinality) grows by doubling, so the final `insert` can transiently hold both the old and new
//! bucket arrays live at once — the single largest contributor to Pass B's peak RSS before the
//! capacity-hint fix noted in `sorted::collect_coords`. Building the MPHF from the exact realized
//! key set and writing directly into a precisely-sized flat array sidesteps both: no load-factor
//! slack, and no growth-transient since the array is allocated once at its final size.
//!
//! Lookups still need the full id per record (not just the MPHF index): `try_hash` isn't
//! collision-free for ids outside the original key set — e.g. a way referencing a node that was
//! never collected.

use boomphf::Mphf;

/// MPHF-indexed, resident array of node coordinate records.
pub struct NodeCoords {
    mphf: Mphf<i64>,
    data: Vec<(i64, f32, f32, bool)>,
}

impl NodeCoords {
    /// Consumes `records`, builds a minimal perfect hash over the ids, and writes each record into
    /// its hashed slot.
    pub fn build(records: Vec<(i64, f32, f32, bool)>) -> Self {
        let len = records.len();
        let ids: Vec<i64> = records.iter().map(|&(id, ..)| id).collect();
        // gamma=1.7 is boomphf's recommended default: a fair space/build-time tradeoff.
        let mphf = Mphf::new(1.7, &ids);
        drop(ids);

        // Placeholder id (never a real OSM id — those are non-negative) so any slot a bug left
        // unwritten reads as "absent" via the same id-mismatch check `get` already does, rather
        // than silently returning zeroed coordinates.
        let mut data = vec![(i64::MIN, 0.0f32, 0.0f32, false); len];
        for (id, lon, lat, shared) in records {
            let idx = mphf.hash(&id) as usize;
            data[idx] = (id, lon, lat, shared);
        }

        NodeCoords { mphf, data }
    }

    pub fn get(&self, id: i64) -> Option<(f32, f32, bool)> {
        // `try_hash` (not `hash`) because `id` may never have been in the collected set — `hash`
        // panics in that case, since it assumes membership; `try_hash` returns `None` once it
        // exhausts all levels without a match. Even the `Some` case is only "arbitrary but in
        // range" for a truly absent key, so the id check below stays load-bearing regardless.
        let idx = self.mphf.try_hash(&id)? as usize;
        let &(rid, lon, lat, shared) = self.data.get(idx)?;
        if rid != id {
            return None;
        }
        Some((lon, lat, shared))
    }

    pub fn iter(&self) -> impl Iterator<Item = (i64, (f32, f32, bool))> + '_ {
        self.data.iter().map(|&(id, lon, lat, shared)| (id, (lon, lat, shared)))
    }
}

/// Accumulates `(id, lon, lat, shared)` coordinate entries during Pass B / the fallback node scan,
/// then produces a finished [`NodeCoords`] by handing the collected records to `NodeCoords::build`.
pub struct NodeCoordsBuilder {
    records: Vec<(i64, f32, f32, bool)>,
}

impl NodeCoordsBuilder {
    pub fn with_capacity(cap: usize) -> Self {
        NodeCoordsBuilder { records: Vec::with_capacity(cap) }
    }

    pub fn insert(&mut self, id: i64, lon: f32, lat: f32, shared: bool) {
        self.records.push((id, lon, lat, shared));
    }

    pub fn finish(self) -> NodeCoords {
        NodeCoords::build(self.records)
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
        let store = NodeCoords::build(records);

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
