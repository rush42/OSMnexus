//! Compact storage for a way's node-id list — `SelectionContext::way_refs`'s value type.
//!
//! A way's `node_refs` used to live as a resident `Vec<i64>` (8 bytes/id) per kept way, for the
//! whole run — on a country-sized extract with tens of millions of ways this rivals or exceeds the
//! node coordinate table in size, yet was untouched by `--disk-node-store` (that only spills
//! `node_coords`, see `disk_coords`'s own doc). Consecutive node ids along a way tend to be close
//! together (the PBF's own `DenseNodes` id encoding banks on the same locality), so delta+zigzag+
//! varint encoding shrinks most real-world ways well below 8 bytes/id, at the cost of a decode pass
//! (linear in the way's length) on every read instead of a slice index.
//!
//! This doesn't reach for the PBF's own on-wire delta encoding directly — `osmpbf`'s `Way::refs()`
//! already fully decodes it into absolute ids before we ever see them (see `resolve::way_data`), so
//! there's no compact representation left to borrow; this re-derives one from the decoded ids
//! instead of trying to bypass `osmpbf`'s way-parsing to reach the original varint bytes.

/// Delta+zigzag+varint encoded node-id list. `EncodedRefs::encode`/`iter` are the only way in/out —
/// callers never see the byte layout.
pub struct EncodedRefs(Box<[u8]>);

impl EncodedRefs {
    pub fn encode(ids: &[i64]) -> Self {
        let mut buf = Vec::with_capacity(ids.len() * 2);
        let mut prev = 0i64;
        for &id in ids {
            // Wrapping, not checked: real OSM ids never come close to i64's edges, but the
            // zigzag/varint round trip is well-defined under two's-complement wraparound regardless
            // (verified by the `i64::MIN`/`i64::MAX` case in this module's own tests), so there's no
            // need to make this fallible over an input shape that can't occur in practice.
            write_varint(&mut buf, zigzag_encode(id.wrapping_sub(prev)));
            prev = id;
        }
        EncodedRefs(buf.into_boxed_slice())
    }

    pub fn iter(&self) -> RefsIter<'_> {
        RefsIter { buf: &self.0, pos: 0, prev: 0 }
    }

    pub fn decode(&self) -> Vec<i64> {
        self.iter().collect()
    }

    /// The way's first and last node id — its graph endpoints. Reading both still requires a full
    /// walk (the last id is a running sum of every delta), so this is no cheaper than `decode()`
    /// asymptotically, but it skips the `Vec` allocation callers that only need endpoints (not the
    /// full geometry) don't want to pay for.
    pub fn first_last(&self) -> Option<(i64, i64)> {
        let mut it = self.iter();
        let first = it.next()?;
        let last = it.last().unwrap_or(first);
        Some((first, last))
    }
}

pub struct RefsIter<'a> {
    buf: &'a [u8],
    pos: usize,
    prev: i64,
}

impl Iterator for RefsIter<'_> {
    type Item = i64;

    fn next(&mut self) -> Option<i64> {
        if self.pos >= self.buf.len() {
            return None;
        }
        let delta = zigzag_decode(read_varint(self.buf, &mut self.pos));
        let id = self.prev.wrapping_add(delta);
        self.prev = id;
        Some(id)
    }
}

fn zigzag_encode(n: i64) -> u64 {
    ((n << 1) ^ (n >> 63)) as u64
}

fn zigzag_decode(u: u64) -> i64 {
    ((u >> 1) as i64) ^ -((u & 1) as i64)
}

fn write_varint(buf: &mut Vec<u8>, mut v: u64) {
    loop {
        let byte = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            buf.push(byte);
            return;
        }
        buf.push(byte | 0x80);
    }
}

/// Reads one varint starting at `*pos`, advancing it past the consumed bytes. Panics on a
/// truncated/malformed buffer — `EncodedRefs` is only ever built by `encode`, so a well-formed
/// buffer is an invariant, not an input to validate.
fn read_varint(buf: &[u8], pos: &mut usize) -> u64 {
    let mut result = 0u64;
    let mut shift = 0u32;
    loop {
        let byte = buf[*pos];
        *pos += 1;
        result |= ((byte & 0x7f) as u64) << shift;
        if byte & 0x80 == 0 {
            return result;
        }
        shift += 7;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_ids() {
        let ids: Vec<i64> = vec![100, 101, 102, 50, 50, 7_000_000_000, -3, 0, i64::MAX, i64::MIN];
        let encoded = EncodedRefs::encode(&ids);
        assert_eq!(encoded.decode(), ids);
    }

    #[test]
    fn first_last_matches_slice_semantics() {
        assert_eq!(EncodedRefs::encode(&[]).first_last(), None);
        assert_eq!(EncodedRefs::encode(&[42]).first_last(), Some((42, 42)));
        assert_eq!(EncodedRefs::encode(&[1, 2, 3]).first_last(), Some((1, 3)));
    }

    #[test]
    fn empty_list_round_trips() {
        let encoded = EncodedRefs::encode(&[]);
        assert_eq!(encoded.decode(), Vec::<i64>::new());
    }
}
