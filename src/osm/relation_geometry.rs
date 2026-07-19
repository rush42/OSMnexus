//! Independent relation-geometry resolution: given a set of way ids needed by kept relations
//! wanting `point`/`line` geometry, re-scans the PBF from scratch for exactly those ways' node
//! refs, then for exactly the node coords they reference — no shared state with the main
//! classify/geometry streaming pass (`osm::reader`). Deliberately a second, self-contained scan
//! rather than piggybacking on the main pass's per-way callback: relation geometry is a small
//! minority of ways in practice, and keeping it fully decoupled means the main pass never needs to
//! know or care whether a way happens to be a relation member.

use anyhow::Context;
use osmpbf::{Element, ElementReader};
use rustc_hash::{FxHashMap, FxHashSet};

/// Re-resolve exactly `needed` ways' coordinate sequences (WGS84 lon/lat), from a fresh scan of
/// the PBF. Unlike `osm::reader::resolve::resolve_geometry`, this has no cut-point/graph concept —
/// relation geometry only needs a plain coordinate sequence per way, not intersection splitting.
/// A way missing from `needed`, or with fewer than 2 resolvable coordinates, is simply absent from
/// the result (mirroring `resolve_geometry`'s own "too short to be a line" behavior).
pub fn resolve_relation_ways(
    path: &str,
    needed: &FxHashSet<i64>,
) -> anyhow::Result<FxHashMap<i64, Vec<(f64, f64)>>> {
    if needed.is_empty() {
        return Ok(FxHashMap::default());
    }

    // Scan 1 — ways: node refs for exactly the needed way ids.
    let way_refs: FxHashMap<i64, Vec<i64>> = ElementReader::from_path(path)
        .context("opening PBF for relation-geometry way scan")?
        .par_map_reduce(
            |element| {
                let mut out: FxHashMap<i64, Vec<i64>> = FxHashMap::default();
                if let Element::Way(way) = element {
                    if needed.contains(&way.id()) {
                        out.insert(way.id(), way.refs().collect());
                    }
                }
                out
            },
            FxHashMap::default,
            |mut a, b| {
                a.extend(b);
                a
            },
        )
        .context("relation-geometry way scan")?;

    // Scan 2 — nodes: coords for exactly the node ids those ways reference.
    let node_ids: FxHashSet<i64> = way_refs.values().flatten().copied().collect();
    let coords: FxHashMap<i64, (f64, f64)> = ElementReader::from_path(path)
        .context("opening PBF for relation-geometry node scan")?
        .par_map_reduce(
            |element| {
                let mut out: FxHashMap<i64, (f64, f64)> = FxHashMap::default();
                match element {
                    Element::DenseNode(n) if node_ids.contains(&n.id()) => {
                        out.insert(n.id(), (n.lon(), n.lat()));
                    }
                    Element::Node(n) if node_ids.contains(&n.id()) => {
                        out.insert(n.id(), (n.lon(), n.lat()));
                    }
                    _ => {}
                }
                out
            },
            FxHashMap::default,
            |mut a, b| {
                a.extend(b);
                a
            },
        )
        .context("relation-geometry node scan")?;

    // Resolve each needed way's node refs into a coordinate sequence, dropping any node missing a
    // coord (matching `resolve_geometry`'s own "skip unresolvable nodes" behavior).
    let mut resolved: FxHashMap<i64, Vec<(f64, f64)>> = FxHashMap::default();
    for (&way_id, refs) in &way_refs {
        let pts: Vec<(f64, f64)> = refs.iter().filter_map(|nid| coords.get(nid).copied()).collect();
        if pts.len() >= 2 {
            resolved.insert(way_id, pts);
        }
    }
    Ok(resolved)
}
