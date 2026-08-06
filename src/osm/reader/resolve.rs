//! Way-filtering, tag extraction, and geometry resolution — the leaf helpers shared by both the
//! sorted fast path and the full-scan fallback.

use std::borrow::Cow;

use rustc_hash::{FxHashMap, FxHashSet};
use osmpbf::{DenseNode, Node, Relation, RelMemberType, Way};
use tracing::info;

use crate::osm::types::{MemberRole, NodeData, OsmWay, RawTags, RelData, WayData, WayMeta};

pub use super::memory_coords::{NodeCoords, NodeCoordsBuilder};

/// Which nodes are referenced by a kept way, from Pass A — the membership test Pass B uses to
/// decide whose coordinates to collect at all (needed for every geometry shape, not just graph
/// output). `Counted` also tracks *how many* ways reference each node, needed only to derive the
/// `shared`/graph-cut-point flag (`count >= 2`) — so when no topic wants graph output
/// (`!needs_graph`, see `Callbacks::needs_graph`), Pass A builds the cheaper `Present` variant
/// instead: a plain id set, half the per-entry size of `Counted`'s `FxHashMap<i64, u32>` (no `u32`
/// payload) and no increment work while indexing ways.
pub(super) enum NodeRefCounts {
    Counted(FxHashMap<i64, u32>),
    Present(FxHashSet<i64>),
}

impl NodeRefCounts {
    pub(super) fn len(&self) -> usize {
        match self {
            NodeRefCounts::Counted(m) => m.len(),
            NodeRefCounts::Present(s) => s.len(),
        }
    }

    pub(super) fn contains_key(&self, id: &i64) -> bool {
        match self {
            NodeRefCounts::Counted(m) => m.contains_key(id),
            NodeRefCounts::Present(s) => s.contains(id),
        }
    }

    /// `Some(shared)` if `id` is a member, `None` otherwise — one lookup, not the separate
    /// `contains_key` + `shared` a caller would otherwise need. `shared` is always `false` for
    /// `Present` (graph output wasn't wanted, so nothing ever reads it for cut-point purposes
    /// anyway).
    pub(super) fn lookup(&self, id: &i64) -> Option<bool> {
        match self {
            NodeRefCounts::Counted(m) => m.get(id).map(|&c| c > 1),
            NodeRefCounts::Present(s) => s.contains(id).then_some(false),
        }
    }
}

pub(super) fn log_node_summary(use_counts: &NodeRefCounts, standalone_classified: u64) {
    match use_counts {
        NodeRefCounts::Counted(m) => {
            let intersections = m.values().filter(|&&c| c >= 2).count();
            info!(
                "{} referenced nodes, {} intersection nodes (≥2 ways), {} standalone nodes classified",
                m.len(),
                intersections,
                standalone_classified
            );
        }
        NodeRefCounts::Present(s) => {
            info!(
                "{} referenced nodes (intersection counts not tracked — no topic wants graph output), \
                 {} standalone nodes classified",
                s.len(),
                standalone_classified
            );
        }
    }
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
///
/// Carries no node refs: `classify` (the only consumer of `WayData`) is purely tag-driven, and the
/// counts/endpoints/`EncodedRefs` bookkeeping that does need refs reads `way.raw_refs()` directly at
/// the Pass A call site instead — `Way::raw_refs()`'s elided lifetime ties its slice to the `&self`
/// borrow of that call, not to `Way<'a>`'s own `'a` (unlike `tags()`, which explicitly returns
/// `'a`), so it can't be stashed into a struct returned from here; it has to be read inline where
/// `way` itself is still in scope. See `way_refs`'s own doc for why that's also the more efficient
/// path (skips `Way::refs()`'s delta-to-absolute summation entirely for the storage path).
pub(super) fn way_data<'a>(way: &Way<'a>) -> WayData<'a> {
    let tags: RawTags<'a> = way.tags().map(|(k, v)| (Cow::Borrowed(k), Cow::Borrowed(v))).collect();
    WayData { id: way.id(), tags, meta: extract_meta(way.info()) }
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
        if let Some((lon, lat, shared)) = coords.get(nid) {
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
