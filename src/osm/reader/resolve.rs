//! Way-filtering, tag extraction, and geometry resolution — the leaf helpers shared by both the
//! sorted fast path and the full-scan fallback.

use std::borrow::Cow;

use rustc_hash::{FxHashMap, FxHashSet};
use osmpbf::{DenseNode, Node, Relation, RelMemberType, Way};
use tracing::info;

use crate::osm::types::{MemberRole, NodeData, OsmWay, RawTags, RelData, WayData, WayMeta};

use super::disk_coords::DiskNodeCoords;
use super::memory_coords::MemoryNodeCoords;
use super::store::MphfFile;

/// Node coordinate map for the geometry pass: `id → (lon, lat, shared)` where `shared` = the node
/// is used by ≥2 filter-passing ways (an intersection cut-point). Folding the shared flag in here
/// is what lets `use_counts` be dropped at the end of Pass B (both paths do so explicitly), so the
/// geometry pass holds only this one map. Part of `SelectionContext` — public since
/// `geom::materialize` (outside `osm::reader`) resolves way/relation geometry from it.
///
/// Both variants are MPHF-indexed record stores (see each module's own doc) differing only in
/// where the record array lives: `Memory` (default) keeps it resident; `Disk` (the
/// `--use-disk-store` opt-in) spills it to an `mmap`'d temp file.
pub enum NodeCoords {
    Memory(MemoryNodeCoords),
    Disk(DiskNodeCoords),
}

impl NodeCoords {
    pub fn get(&self, id: i64) -> Option<(f32, f32, bool)> {
        match self {
            NodeCoords::Memory(m) => m.get(id),
            NodeCoords::Disk(d) => d.get(id),
        }
    }

    pub fn len(&self) -> usize {
        match self {
            NodeCoords::Memory(m) => m.len(),
            NodeCoords::Disk(d) => d.len(),
        }
    }

    pub fn iter(&self) -> Box<dyn Iterator<Item = (i64, (f32, f32, bool))> + '_> {
        match self {
            NodeCoords::Memory(m) => Box::new(m.iter()),
            NodeCoords::Disk(d) => Box::new(d.iter()),
        }
    }
}

/// Accumulates `(id, lon, lat, shared)` coordinate entries during Pass B / the fallback node scan,
/// then produces a finished [`NodeCoords`] by handing the collected records to whichever backend's
/// `build` — both build their MPHF from this exact record set, so entries can arrive in any order.
pub struct NodeCoordsBuilder {
    disk: bool,
    records: Vec<(i64, f32, f32, bool)>,
}

impl NodeCoordsBuilder {
    pub fn with_capacity(disk: bool, cap: usize) -> Self {
        NodeCoordsBuilder { disk, records: Vec::with_capacity(cap) }
    }

    pub fn insert(&mut self, id: i64, lon: f32, lat: f32, shared: bool) {
        self.records.push((id, lon, lat, shared));
    }

    pub fn finish(self) -> anyhow::Result<NodeCoords> {
        if self.disk {
            Ok(NodeCoords::Disk(DiskNodeCoords::build(self.records)?))
        } else {
            Ok(NodeCoords::Memory(MemoryNodeCoords::build(self.records)))
        }
    }
}

/// Which nodes are referenced by a kept way, from Pass A — the membership test Pass B uses to
/// decide whose coordinates to collect at all (needed for every geometry shape, not just graph
/// output). `Counted` also tracks *how many* ways reference each node, needed only to derive the
/// `shared`/graph-cut-point flag (`count >= 2`) — so when no topic wants graph output
/// (`!needs_graph`, see `Callbacks::needs_graph`), Pass A builds the cheaper `Present` variant
/// instead: a plain id set, half the per-entry size of `Counted`'s `FxHashMap<i64, u32>` (no `u32`
/// payload) and no increment work while indexing ways.
/// `Disk` stores the same `count: u32` payload as `Counted` (a `Present`-derived store just fills
/// every id's payload with `0`, since `lookup`'s `c > 1` check already reads as "never shared" for
/// that sentinel — the two source shapes collapse to identical `Disk` behavior for `lookup`, though
/// `log_node_summary` still needs to tell them apart, hence the `counted` flag). Built once, at the
/// end of Pass A, from whichever resident accumulator the caller used during indexing — the MPHF
/// still needs the full key set to build (see `store`'s own doc), so there's no way to skip the
/// resident accumulation step entirely, only to drop it once the compact backend exists.
pub(super) enum NodeRefCounts {
    Counted(FxHashMap<i64, u32>),
    Present(FxHashSet<i64>),
    Disk(MphfFile<u32>, bool),
}

impl NodeRefCounts {
    /// Finishes a `Counted` accumulation: resident `FxHashMap` (default) or spilled to a
    /// `use_disk_store`-gated `MphfFile` (dropping the map).
    pub(super) fn from_counted(counts: FxHashMap<i64, u32>, use_disk_store: bool) -> anyhow::Result<Self> {
        if use_disk_store {
            let records: Vec<(i64, u32)> = counts.into_iter().collect();
            Ok(NodeRefCounts::Disk(MphfFile::build(records)?, true))
        } else {
            Ok(NodeRefCounts::Counted(counts))
        }
    }

    /// Finishes a `Present` accumulation: resident `FxHashSet` (default) or spilled to a
    /// `use_disk_store`-gated `MphfFile`, with every id's payload set to the "never shared" sentinel.
    pub(super) fn from_present(present: FxHashSet<i64>, use_disk_store: bool) -> anyhow::Result<Self> {
        if use_disk_store {
            let records: Vec<(i64, u32)> = present.into_iter().map(|id| (id, 0u32)).collect();
            Ok(NodeRefCounts::Disk(MphfFile::build(records)?, false))
        } else {
            Ok(NodeRefCounts::Present(present))
        }
    }

    pub(super) fn len(&self) -> usize {
        match self {
            NodeRefCounts::Counted(m) => m.len(),
            NodeRefCounts::Present(s) => s.len(),
            NodeRefCounts::Disk(m, _) => m.len(),
        }
    }

    pub(super) fn contains_key(&self, id: &i64) -> bool {
        match self {
            NodeRefCounts::Counted(m) => m.contains_key(id),
            NodeRefCounts::Present(s) => s.contains(id),
            NodeRefCounts::Disk(m, _) => m.get(*id).is_some(),
        }
    }

    /// `Some(shared)` if `id` is a member, `None` otherwise — one lookup, not the separate
    /// `contains_key` + `shared` a caller would otherwise need. `shared` is always `false` for
    /// `Present` (graph output wasn't wanted, so nothing ever reads it for cut-point purposes
    /// anyway) and for a `Disk` store built from `Present` (sentinel payload `0`, never `> 1`).
    pub(super) fn lookup(&self, id: &i64) -> Option<bool> {
        match self {
            NodeRefCounts::Counted(m) => m.get(id).map(|&c| c > 1),
            NodeRefCounts::Present(s) => s.contains(id).then_some(false),
            NodeRefCounts::Disk(m, _) => m.get(*id).map(|c| c > 1),
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
        NodeRefCounts::Disk(m, counted) => {
            if *counted {
                let intersections = m.iter().filter(|&(_, c)| c >= 2).count();
                info!(
                    "{} referenced nodes, {} intersection nodes (≥2 ways), {} standalone nodes classified",
                    m.len(),
                    intersections,
                    standalone_classified
                );
            } else {
                info!(
                    "{} referenced nodes (intersection counts not tracked — no topic wants graph output), \
                     {} standalone nodes classified",
                    m.len(),
                    standalone_classified
                );
            }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disk_backed_counted_matches_memory_semantics() {
        let mut counts = FxHashMap::default();
        counts.insert(10i64, 1u32);
        counts.insert(20, 2);
        counts.insert(7_000_000_000, 5);

        let disk = NodeRefCounts::from_counted(counts.clone(), true).unwrap();
        let mem = NodeRefCounts::from_counted(counts, false).unwrap();

        for id in [10i64, 20, 7_000_000_000, 999] {
            assert_eq!(disk.lookup(&id), mem.lookup(&id), "mismatch for id {id}");
            assert_eq!(disk.contains_key(&id), mem.contains_key(&id), "mismatch for id {id}");
        }
        assert_eq!(disk.len(), mem.len());
    }

    #[test]
    fn disk_backed_present_never_reports_shared() {
        let mut present = FxHashSet::default();
        present.insert(10i64);
        present.insert(20);

        let disk = NodeRefCounts::from_present(present, true).unwrap();
        assert_eq!(disk.lookup(&10), Some(false));
        assert_eq!(disk.lookup(&20), Some(false));
        assert_eq!(disk.lookup(&999), None);
        assert!(disk.contains_key(&10));
        assert!(!disk.contains_key(&999));
    }
}
