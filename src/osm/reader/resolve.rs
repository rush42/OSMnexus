//! Way-filtering, tag extraction, and geometry resolution — the leaf helpers shared by both the
//! sorted fast path and the full-scan fallback.

use std::borrow::Cow;

use rustc_hash::{FxHashMap, FxHashSet};
use osmpbf::{DenseNode, Node, Relation, RelMemberType, Way};
use tracing::info;

use crate::osm::types::{MemberRole, NodeData, OsmWay, RawTags, RelData, WayData, WayMeta};

/// Node coordinate map for the geometry pass: `id → (lon, lat, shared)` where `shared` = the node
/// is used by ≥2 filter-passing ways (an intersection cut-point). Folding the shared flag in here
/// is what lets `use_counts` be dropped at the end of Pass B (both paths do so explicitly), so the
/// geometry pass holds only this one map. Part of `SelectionContext` — public since
/// `geom::materialize` (outside `osm::reader`) resolves way/relation geometry from it.
pub type NodeCoords = FxHashMap<i64, (f32, f32, bool)>;

pub(super) fn log_node_summary(use_counts: &FxHashMap<i64, u32>, standalone_classified: u64) {
    let intersections = use_counts.values().filter(|&&c| c >= 2).count();
    info!(
        "{} referenced nodes, {} intersection nodes (≥2 ways), {} standalone nodes classified",
        use_counts.len(),
        intersections,
        standalone_classified
    );
}

/// Extract element metadata (timestamp/user/changeset) from an osmpbf `Info`, or an empty `WayMeta`
/// when the element carries no version info. Shared by way/relation/node extraction.
fn extract_meta(info: osmpbf::Info) -> WayMeta {
    if info.version().is_some() {
        WayMeta {
            timestamp: info.milli_timestamp().map(|ms| ms / 1000),
            user: info.user().and_then(|r| r.ok()).map(|s| s.to_owned()),
            changeset: info.changeset(),
        }
    } else {
        WayMeta { timestamp: None, user: None, changeset: None }
    }
}

/// Extract a [`WayData`] from an osmpbf `Way`. `tags` borrows straight from the block's string
/// table (`Cow::Borrowed`, no allocation) — an excluded way (kept by no topic) never pays for a
/// tag clone at all; see `RawTags`'s own doc.
pub(super) fn way_data<'a>(way: &Way<'a>) -> WayData<'a> {
    let tags: RawTags<'a> = way.tags().map(|(k, v)| (Cow::Borrowed(k), Cow::Borrowed(v))).collect();
    let refs: Vec<i64> = way.refs().collect();
    WayData { id: way.id(), tags, node_refs: refs, meta: extract_meta(way.info()) }
}

/// Extract a [`RelData`] from an osmpbf `Relation`, keeping only its **way** members (id + role —
/// role is needed for `Polygon` assembly, see `geom::relation`).
pub(super) fn rel_data<'a>(rel: &Relation<'a>) -> RelData<'a> {
    let tags: RawTags<'a> = rel.tags().map(|(k, v)| (Cow::Borrowed(k), Cow::Borrowed(v))).collect();
    let member_ways: Vec<(i64, MemberRole)> = rel
        .members()
        .filter(|m| m.member_type == RelMemberType::Way)
        .map(|m| (m.member_id, MemberRole::from_str(m.role().unwrap_or(""))))
        .collect();
    RelData { id: rel.id(), tags, member_ways, meta: extract_meta(rel.info()) }
}

/// Extract a [`NodeData`] from an osmpbf dense node. Dense nodes carry `DenseNodeInfo` (distinct
/// from the `Info` on ways/relations/sparse nodes), present only when the file has metadata.
pub(super) fn dense_node_data<'a>(n: &DenseNode<'a>) -> NodeData<'a> {
    let tags: RawTags<'a> = n.tags().map(|(k, v)| (Cow::Borrowed(k), Cow::Borrowed(v))).collect();
    let meta = match n.info() {
        Some(info) => WayMeta {
            timestamp: Some(info.milli_timestamp() / 1000),
            user: info.user().ok().map(|s| s.to_owned()),
            changeset: Some(info.changeset()),
        },
        None => WayMeta { timestamp: None, user: None, changeset: None },
    };
    NodeData { id: n.id(), tags, meta, lon: n.lon(), lat: n.lat() }
}

/// Extract a [`NodeData`] from an osmpbf (non-dense) node.
pub(super) fn node_data<'a>(n: &Node<'a>) -> NodeData<'a> {
    let tags: RawTags<'a> = n.tags().map(|(k, v)| (Cow::Borrowed(k), Cow::Borrowed(v))).collect();
    NodeData { id: n.id(), tags, meta: extract_meta(n.info()), lon: n.lon(), lat: n.lat() }
}

/// Resolve a way's node ids into an `OsmWay` geometry by looking up node coordinates. Tag/meta are
/// not involved — classification already ran in Pass A.
///
/// `selected` holds node ids classified in the nodes pass by a topic that also wants node `graph`
/// output (e.g. crossings, signals — see `GeometryPlan::node_graph_mask`); they become forced cut
/// points ("count as intersections") even when used by a single way. A node classified only by a
/// point-only node topic is not in `selected` and has no effect on cutting.
pub fn resolve_geometry(
    id: i64,
    node_refs: &[i64],
    coords: &NodeCoords,
    selected: &FxHashSet<i64>,
) -> Option<OsmWay> {
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

    // Cut points: start + end (always), interior nodes shared with another way, and any node
    // selected by a node topic (a graph vertex even at use-count 1).
    let last = kept.len() - 1;
    let mut cut_points: Vec<(u32, i64)> = Vec::new();
    for (i, &(nid, shared)) in kept.iter().enumerate() {
        if i == 0 || i == last || shared || selected.contains(&nid) {
            cut_points.push((i as u32, nid));
        }
    }

    Some(OsmWay { id, coords: pts, cut_points })
}
