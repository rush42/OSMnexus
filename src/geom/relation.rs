//! Relation-geometry assembly. Resolving a relation's member ways' coordinates is *not* done here
//! — it's `osm::reader`'s `extra_way_ids`/`build_extra_geom` side channel (see `Callbacks`'s own
//! doc): the reader's Pass A/B (or fallback scans 1/2) already decode every way and every needed
//! node once, so relation-geometry member ways just ride along in that same decode, needing no
//! second PBF scan. What's left here is purely the geometric assembly from the resolved
//! coordinates: chaining member ways into rings for `Polygon`.

/// Chain a set of way coordinate sequences end-to-end into one or more closed rings — the
/// multipolygon convention's `outer`/`inner` member ways are frequently split across several ways
/// (e.g. a building outline shared with neighbors), so `Polygon` assembly needs to reconnect them
/// by matching endpoints before a ring exists at all. Endpoint matching is exact-coordinate
/// equality: a shared OSM node id always resolves to the identical `(f64,f64)` (same map lookup in
/// `osm::reader::resolve_extra_way_coords`), so no epsilon tolerance is needed.
///
/// Greedy: starts a ring from an arbitrary remaining segment, repeatedly extends it by any
/// remaining segment whose start or end matches the ring's current end (reversing the segment if
/// needed), until the ring closes (first == last) or no more segments match — at which point that
/// ring is done (closed or not; an unclosed ring still gets force-closed by the caller, same as a
/// single-way polygon) and a new ring starts from whatever remains. Doesn't attempt to spatially
/// assign which hole belongs to which outer shape when there are multiple disjoint outer rings —
/// see `geom::builders::build_relation_polygon_row`'s own doc for that simplification.
pub fn assemble_rings(mut segments: Vec<Vec<(f64, f64)>>) -> Vec<Vec<(f64, f64)>> {
    let mut rings = Vec::new();
    while let Some(mut ring) = segments.pop() {
        loop {
            if ring.first() == ring.last() {
                break; // already closed — don't keep chaining onto a closed ring
            }
            let last = *ring.last().unwrap();
            let next = segments.iter().position(|s| s.first() == Some(&last) || s.last() == Some(&last));
            match next {
                Some(i) => {
                    let mut seg = segments.remove(i);
                    if seg.first() == Some(&last) {
                        ring.extend(seg.drain(1..));
                    } else {
                        seg.reverse();
                        ring.extend(seg.drain(1..));
                    }
                }
                None => break, // nothing left connects — done with this ring
            }
        }
        rings.push(ring);
    }
    rings
}
