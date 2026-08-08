//! Node coordinate store — MPHF-indexed, resident array of `(id, lon, lat, shared)` records, built
//! once from every collected coordinate.
//!
//! Replaces a plain `FxHashMap<i64, (f32,f32,bool)>`: a hashmap has to reserve slack for its load
//! factor and store the full key in every bucket regardless, and (worse for peak RSS at this
//! cardinality) grows by doubling, so the final `insert` can transiently hold both the old and new
//! bucket arrays live at once — the single largest contributor to Pass B's peak RSS before the
//! capacity-hint fix noted in `sorted::collect_coords`. Building the MPHF from the exact realized
//! key set and writing directly into a precisely-sized flat array sidesteps both: no load-factor
//! slack, and no growth-transient since the array is allocated once at its final size — and, since
//! the MPHF is a bijection onto `0..len`, no *second* array either: `build` permutes the collected
//! records into slot order in place (see its own doc).
//!
//! Lookups still need the full id per record (not just the MPHF index): `try_hash` isn't
//! collision-free for ids outside the original key set — e.g. a way referencing a node that was
//! never collected.

use boomphf::Mphf;

/// Scale of the stored fixed-point coordinates: decimicrodegrees (10⁻⁷°), the PBF's own native
/// integer representation (`osmpbf`'s `decimicro_lat`/`decimicro_lon`).
pub const DECIMICRO: f64 = 1e-7;

/// MPHF-indexed, resident array of node coordinate records.
///
/// Coordinates are stored as fixed-point `i32` decimicrodegrees rather than `f32` degrees. Same 4
/// bytes each, but: `f32`'s 24-bit mantissa only resolves ~2 m at the longitude range's top end
/// (and every value paid a lossy `f64 -> f32` narrowing on the way in), whereas the `i32` form is
/// the exact integer the PBF already encodes — no conversion, no loss, ~1 cm resolution.
///
/// `shared` lives in a sidecar bitset instead of the record: as a `bool` field it forced the tuple
/// to 8-byte alignment padding, costing 8 bytes per record to carry one bit (24 B/record vs the
/// 16 B the `(i64, i32, i32)` payload actually needs). At Germany's ~76M referenced nodes that
/// padding alone was ~600 MB.
pub struct NodeCoords {
    mphf: Mphf<i64>,
    data: Vec<(i64, i32, i32)>,
    shared: Vec<u64>,
}

/// `bits[i]` — the visited-slot bitset used by `build`'s in-place permutation.
#[inline]
fn bit_is_set(bits: &[u64], i: usize) -> bool {
    bits[i / 64] >> (i % 64) & 1 == 1
}

#[inline]
fn bit_set(bits: &mut [u64], i: usize) {
    bits[i / 64] |= 1 << (i % 64);
}

#[inline]
fn bit_assign(bits: &mut [u64], i: usize, value: bool) {
    if value {
        bits[i / 64] |= 1 << (i % 64);
    } else {
        bits[i / 64] &= !(1 << (i % 64));
    }
}

impl NodeCoords {
    /// Consumes `records`, builds a minimal perfect hash over the ids, and permutes `records` into
    /// MPHF slot order **in place**.
    ///
    /// `records` and `shared` are parallel arrays in insertion order (`shared`'s bit `i` belongs to
    /// `records[i]`); both are permuted together into slot order.
    ///
    /// The obvious implementation — allocate a second `Vec` of the same length and write each
    /// record into `data[mphf.hash(id)]` — holds both vectors live at once for the whole scatter
    /// loop (a `Vec`'s buffer isn't returned until its `IntoIter` drops, so moving elements out one
    /// at a time frees nothing along the way). At this cardinality that doubling is the single
    /// largest transient in the build: on a Germany extract, ~76M records × 16 B = ~1.2 GB of pure
    /// duplication, landing exactly when the process is already at its high-water mark.
    ///
    /// Since a *minimal perfect* hash is a bijection from the key set onto `0..len`, the target
    /// layout is just `records` permuted — so it can be applied in place by cycle-following, with
    /// only a visited bitset (`len` bits ≈ 9 MB at that same cardinality) instead of a second
    /// records-sized array.
    ///
    /// Requires ids to be unique — already a precondition of `Mphf::new` itself, and guaranteed
    /// here since each node id is collected at most once per run. A duplicate would make the map
    /// non-bijective (two records competing for one slot); that's caught below rather than left to
    /// corrupt the permutation silently.
    pub fn build(mut records: Vec<(i64, i32, i32)>, mut shared: Vec<u64>) -> Self {
        let len = records.len();
        let ids: Vec<i64> = records.iter().map(|&(id, ..)| id).collect();
        // gamma=1.7 is boomphf's recommended default: a fair space/build-time tradeoff.
        let mphf = Mphf::new(1.7, &ids);
        drop(ids);

        let mut placed = vec![0u64; (len + 63) / 64];
        for start in 0..len {
            if bit_is_set(&placed, start) {
                continue;
            }
            // Walk this cycle: carry one record (and its `shared` bit) at a time, swapping it into
            // its slot and picking up whatever it displaced, until the chain loops back to where it
            // started.
            let mut item = records[start];
            let mut carry = bit_is_set(&shared, start);
            let mut steps = 0usize;
            loop {
                let dest = mphf.hash(&item.0) as usize;
                if dest == start {
                    records[start] = item;
                    bit_assign(&mut shared, start, carry);
                    bit_set(&mut placed, start);
                    break;
                }
                assert!(
                    !bit_is_set(&placed, dest),
                    "NodeCoords::build: slot {dest} claimed twice — duplicate node id in records"
                );
                bit_set(&mut placed, dest);
                std::mem::swap(&mut records[dest], &mut item);
                let displaced = bit_is_set(&shared, dest);
                bit_assign(&mut shared, dest, carry);
                carry = displaced;
                steps += 1;
                assert!(steps <= len, "NodeCoords::build: permutation cycle did not terminate");
            }
        }

        NodeCoords { mphf, data: records, shared }
    }

    /// `(lon, lat)` in degrees plus the `shared` flag. Degrees are reconstructed from the stored
    /// fixed-point form here so callers keep working in the units they always did.
    pub fn get(&self, id: i64) -> Option<(f64, f64, bool)> {
        // `try_hash` (not `hash`) because `id` may never have been in the collected set — `hash`
        // panics in that case, since it assumes membership; `try_hash` returns `None` once it
        // exhausts all levels without a match. Even the `Some` case is only "arbitrary but in
        // range" for a truly absent key, so the id check below stays load-bearing regardless.
        let idx = self.mphf.try_hash(&id)? as usize;
        let &(rid, lon, lat) = self.data.get(idx)?;
        if rid != id {
            return None;
        }
        Some((lon as f64 * DECIMICRO, lat as f64 * DECIMICRO, bit_is_set(&self.shared, idx)))
    }

    pub fn iter(&self) -> impl Iterator<Item = (i64, (f64, f64, bool))> + '_ {
        self.data.iter().enumerate().map(|(slot, &(id, lon, lat))| {
            (id, (lon as f64 * DECIMICRO, lat as f64 * DECIMICRO, bit_is_set(&self.shared, slot)))
        })
    }
}

/// Accumulates `(id, lon, lat, shared)` coordinate entries during Pass B / the fallback node scan,
/// then produces a finished [`NodeCoords`] by handing the collected records to `NodeCoords::build`.
/// The accumulating record array is the same 16-byte shape the finished store keeps, with `shared`
/// in a parallel bitset rather than a `bool` field — so `finish` hands its buffers straight to
/// `NodeCoords::build`, which permutes them in place. Carrying `shared` inside the tuple instead
/// would pad each record back to 24 bytes *and* force a shrink-to-16 copy at the end, holding both
/// sizes live at once — precisely the transient `build`'s in-place permutation exists to avoid.
pub struct NodeCoordsBuilder {
    records: Vec<(i64, i32, i32)>,
    shared: Vec<u64>,
}

impl NodeCoordsBuilder {
    pub fn with_capacity(cap: usize) -> Self {
        NodeCoordsBuilder {
            records: Vec::with_capacity(cap),
            shared: Vec::with_capacity((cap + 63) / 64),
        }
    }

    /// `lon`/`lat` are decimicrodegrees (10⁻⁷°) — `osmpbf`'s `decimicro_lon`/`decimicro_lat`,
    /// passed straight through without a float roundtrip.
    pub fn insert(&mut self, id: i64, lon: i32, lat: i32, shared: bool) {
        let idx = self.records.len();
        self.records.push((id, lon, lat));
        if idx % 64 == 0 {
            self.shared.push(0);
        }
        if shared {
            bit_set(&mut self.shared, idx);
        }
    }

    pub fn finish(self) -> NodeCoords {
        NodeCoords::build(self.records, self.shared)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Degrees for a decimicrodegree literal, matching what `get`/`iter` reconstruct.
    fn deg(dm: i32) -> f64 {
        dm as f64 * DECIMICRO
    }

    /// Build from `(id, lon, lat, shared)` tuples via the builder — the only supported way in, and
    /// what every caller actually uses.
    fn build(records: &[(i64, i32, i32, bool)]) -> NodeCoords {
        let mut b = NodeCoordsBuilder::with_capacity(records.len());
        for &(id, lon, lat, shared) in records {
            b.insert(id, lon, lat, shared);
        }
        b.finish()
    }

    #[test]
    fn round_trips_present_ids_and_rejects_absent_ones() {
        let records = vec![
            (10i64, 10_000_000i32, 20_000_000i32, false),
            (20, 30_000_000, 40_000_000, true),
            (30, -50_000_000, 60_000_000, false),
            (7_000_000_000, 1_800_000_000, -900_000_000, true),
        ];
        let store = build(&records);

        assert_eq!(store.get(10), Some((deg(10_000_000), deg(20_000_000), false)));
        assert_eq!(store.get(20), Some((deg(30_000_000), deg(40_000_000), true)));
        assert_eq!(store.get(30), Some((deg(-50_000_000), deg(60_000_000), false)));
        assert_eq!(
            store.get(7_000_000_000),
            Some((deg(1_800_000_000), deg(-900_000_000), true))
        );

        assert_eq!(store.get(11), None);
        assert_eq!(store.get(0), None);
        assert_eq!(store.get(-5), None);

        let mut collected: Vec<_> = store.iter().collect();
        collected.sort_by_key(|&(id, _)| id);
        assert_eq!(
            collected,
            vec![
                (10, (deg(10_000_000), deg(20_000_000), false)),
                (20, (deg(30_000_000), deg(40_000_000), true)),
                (30, (deg(-50_000_000), deg(60_000_000), false)),
                (7_000_000_000, (deg(1_800_000_000), deg(-900_000_000), true)),
            ]
        );
    }

    /// The `shared` flag moved out of the record into a sidecar bitset addressed by MPHF slot —
    /// so it has to survive the in-place permutation still attached to the right node.
    #[test]
    fn shared_flag_tracks_the_right_node_across_the_permutation() {
        let records: Vec<(i64, i32, i32, bool)> =
            (0..500i64).map(|i| (i * 3 + 1, i as i32, -(i as i32), i % 2 == 0)).collect();
        let store = build(&records);
        for &(id, lon, lat, shared) in &records {
            assert_eq!(store.get(id), Some((deg(lon), deg(lat), shared)), "id {id}");
        }
    }

    #[test]
    fn empty_store_builds_and_looks_up() {
        let store = build(&[]);
        assert_eq!(store.get(1), None);
        assert_eq!(store.iter().count(), 0);
    }

    #[test]
    fn single_record() {
        let store = build(&[(42i64, 15_000_000i32, 25_000_000i32, true)]);
        assert_eq!(store.get(42), Some((deg(15_000_000), deg(25_000_000), true)));
        assert_eq!(store.get(43), None);
    }

    /// The in-place permutation is cycle-following, so a build large enough to contain many
    /// multi-element cycles (and to cross the bitset's 64-slot word boundaries) is the case that
    /// would expose an off-by-one or a mis-tracked visited slot.
    #[test]
    fn many_records_all_round_trip_after_in_place_permutation() {
        let records: Vec<(i64, i32, i32, bool)> = (0..1000i64)
            .map(|i| (i * 7 + 1, i as i32, (i * 2) as i32, i % 3 == 0))
            .collect();
        let store = build(&records);

        for &(id, lon, lat, shared) in &records {
            assert_eq!(
                store.get(id),
                Some((deg(lon), deg(lat), shared)),
                "id {id} lost or corrupted"
            );
        }
        // Every record is present exactly once — a broken permutation could duplicate one and drop
        // another while still passing the per-id lookups above.
        assert_eq!(store.iter().count(), records.len());
        let mut seen: Vec<i64> = store.iter().map(|(id, _)| id).collect();
        seen.sort_unstable();
        let mut expected: Vec<i64> = records.iter().map(|&(id, ..)| id).collect();
        expected.sort_unstable();
        assert_eq!(seen, expected);
    }
}
