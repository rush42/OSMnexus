//! Way-filtering, tag extraction, and geometry resolution — the leaf helpers shared by both the
//! sorted fast path and the full-scan fallback.

use rustc_hash::FxHashMap;
use osmpbf::Way;
use tracing::info;

use crate::osm::types::{ElementFilter, OsmWay, RawTags, WayData, WayMeta};

/// Node coordinate map for the geometry pass: `id → (lon, lat, shared)` where `shared` = the node
/// is used by ≥2 filter-passing ways (an intersection cut-point). Folding the shared flag in here
/// lets `use_counts` be dropped after Pass B, so the geometry pass holds only this one map.
pub(super) type NodeCoords = FxHashMap<i64, (f32, f32, bool)>;

pub(super) fn log_node_summary(use_counts: &FxHashMap<i64, u32>) {
    let intersections = use_counts.values().filter(|&&c| c >= 2).count();
    info!(
        "{} referenced nodes, {} intersection nodes (≥2 ways)",
        use_counts.len(),
        intersections
    );
}

/// True if the way matches any topic's element filter.
pub(super) fn way_passes(filters: &[ElementFilter], way: &Way) -> bool {
    filters.iter().any(|f| {
        way.tags().any(|(k, v)| {
            k == f.tag
                && match &f.any_of {
                    None => true,
                    Some(allowed) => allowed.iter().any(|a| a == v),
                }
        })
    })
}

/// Extract a [`WayData`] from an osmpbf `Way`.
pub(super) fn way_data(way: &Way) -> WayData {
    let tags: RawTags = way.tags().map(|(k, v)| (k.to_owned(), v.to_owned())).collect();
    let refs: Vec<i64> = way.refs().collect();

    let meta = if way.info().version().is_some() {
        let info = way.info();
        WayMeta {
            timestamp: info.milli_timestamp().map(|ms| ms / 1000),
            user: info.user().and_then(|r| r.ok()).map(|s| s.to_owned()),
            changeset: info.changeset(),
        }
    } else {
        WayMeta { timestamp: None, user: None, changeset: None }
    };

    WayData { id: way.id(), tags, node_refs: refs, meta }
}

/// Resolve a way's node ids into an `OsmWay` geometry by looking up node coordinates. Tag/meta are
/// not involved — classification already ran in Pass A.
pub(super) fn resolve_geometry(id: i64, node_refs: &[i64], coords: &NodeCoords) -> Option<OsmWay> {
    // One pass: keep only nodes that have coords, tracking their id + shared flag so cut points stay
    // aligned to `pts` indices (a dropped missing-coord node must not shift the indices).
    let mut pts: Vec<(f64, f64)> = Vec::with_capacity(node_refs.len());
    let mut kept: Vec<(i64, bool)> = Vec::with_capacity(node_refs.len());
    for &nid in node_refs {
        if let Some(&(lon, lat, shared)) = coords.get(&nid) {
            pts.push((lon as f64, lat as f64));
            kept.push((nid, shared));
        }
    }

    if pts.len() < 2 {
        return None;
    }

    // Cut points: start + end (always), plus interior nodes shared with another way.
    let last = kept.len() - 1;
    let mut cut_points: Vec<(u32, i64)> = Vec::new();
    for (i, &(nid, shared)) in kept.iter().enumerate() {
        if i == 0 || i == last || shared {
            cut_points.push((i as u32, nid));
        }
    }

    Some(OsmWay { id, coords: pts, cut_points })
}
