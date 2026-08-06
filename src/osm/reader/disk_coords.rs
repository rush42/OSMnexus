//! Disk-backed alternative to the in-memory `NodeCoords` hashmap, opt in via `--use-disk-store`.
//!
//! The in-memory map (`FxHashMap<i64, (f32,f32,bool)>`) stays resident for the whole run — through
//! Pass B *and* the materialize phase, alongside `way_refs` and `resolved` (see `geom::materialize`'s
//! own doc) — which is the dominant memory cost on a big extract. This trades that permanent
//! residency for a one-time minimal-perfect-hash build plus a flat, hash-indexed record file that's
//! `mmap`'d read-only, so the OS pages it in/out under memory pressure instead of it being pinned RSS.
//!
//! A thin wrapper over `store::MphfFile` — see that module's doc for the shared MPHF/mmap mechanics.
//! Record layout (17 bytes): `id: i64 LE | lon: f32 LE | lat: f32 LE | shared: u8`.

use super::store::{FixedPayload, MphfFile};

#[derive(Copy, Clone)]
struct CoordPayload {
    lon: f32,
    lat: f32,
    shared: bool,
}

impl FixedPayload for CoordPayload {
    const LEN: usize = 9;

    fn encode(&self, buf: &mut [u8]) {
        buf[0..4].copy_from_slice(&self.lon.to_le_bytes());
        buf[4..8].copy_from_slice(&self.lat.to_le_bytes());
        buf[8] = self.shared as u8;
    }

    fn decode(buf: &[u8]) -> Self {
        CoordPayload {
            lon: f32::from_le_bytes(buf[0..4].try_into().unwrap()),
            lat: f32::from_le_bytes(buf[4..8].try_into().unwrap()),
            shared: buf[8] != 0,
        }
    }
}

/// MPHF-indexed, `mmap`'d flat file of node coordinate records, built once from every collected
/// `(id, lon, lat, shared)` tuple.
pub struct DiskNodeCoords(MphfFile<CoordPayload>);

impl DiskNodeCoords {
    /// Consumes `records`, builds a minimal perfect hash over the ids, writes hash-indexed records
    /// to a temp file, and `mmap`s it read-only.
    pub fn build(records: Vec<(i64, f32, f32, bool)>) -> anyhow::Result<Self> {
        let records = records
            .into_iter()
            .map(|(id, lon, lat, shared)| (id, CoordPayload { lon, lat, shared }))
            .collect();
        Ok(DiskNodeCoords(MphfFile::build(records)?))
    }

    pub fn get(&self, id: i64) -> Option<(f32, f32, bool)> {
        self.0.get(id).map(|p| (p.lon, p.lat, p.shared))
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn iter(&self) -> impl Iterator<Item = (i64, (f32, f32, bool))> + '_ {
        self.0.iter().map(|(id, p)| (id, (p.lon, p.lat, p.shared)))
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
