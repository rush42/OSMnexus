//! Topic-agnostic PBF streaming reader — the "select" phase. Drives tag classification of
//! relations, ways, and nodes over a PBF (the sorted fast path decodes each blob region once, in
//! the logical order relations → ways → nodes; an unsorted file falls back to full scans), and
//! returns a [`SelectionContext`]: everything geometry construction needs (node coordinates, each
//! kept way's node refs, each kept relation's member ways, node use-counts for graph-vertex
//! detection) — but no geometry itself. That's `geom::materialize`'s job, run separately
//! afterward over the returned context; this module never builds or routes a geometry row.
//!
//! Tag-row output is the one thing that *can't* wait for a returned value — buffering every tag
//! row in memory until the whole file is classified is exactly what an earlier version of this
//! pipeline did and OOM'd on a country-sized import (see the project history). So `classify_rel`/
//! `classify_way`/`classify_node` still stream their tag rows out as a side effect while the file
//! is read; only the (much smaller — bounded by kept elements' id/node-ref data, not tag content)
//! geometry inputs are buffered and returned.

mod blob_index;
mod fallback;
mod resolve;
mod sorted;

use anyhow::Context;
use rustc_hash::{FxHashMap, FxHashSet};
use tracing::{info, warn};

use crate::osm::types::{MemberRole, NodeData, RelData, WayData};

use blob_index::{build_blob_index, find_relation_section_start, find_way_section_start, pbf_is_sorted};
use fallback::stream_osm_fallback;
pub use resolve::{resolve_geometry, NodeCoords};
use sorted::{classify_and_index, classify_relations, collect_coords};

/// Everything geometry construction needs, once the "select" phase (relations → ways → nodes)
/// finishes — see this module's own doc for why it's returned as one value instead of driven via
/// per-element geometry callbacks the way tag classification still is.
pub struct SelectionContext {
    /// Every node referenced by a `way_refs` entry (kept way or relation-member way alike).
    pub node_coords: NodeCoords,
    /// `way_id -> (raw node refs, per-topic keep mask)`. `mask == 0` means the way was never
    /// tag-kept by any topic — it's here purely because some kept relation in `rel_members`
    /// references it, so its own shapes (line/point/polygon/graph) are never built, only its
    /// coordinates are available for relation-geometry assembly.
    pub way_refs: FxHashMap<i64, (Vec<i64>, u32)>,
    /// `relation_id -> (member ways with role, per-topic keep mask)`, for every tag-kept relation
    /// (regardless of whether any topic wants relation geometry — that decision is
    /// `geom::materialize`'s, using a `GeometryPlan`, not this module's).
    pub rel_members: FxHashMap<i64, (Vec<(i64, MemberRole)>, u32)>,
    /// Per-node reference counts among `way_refs` entries with `mask != 0` — a node used by ≥2
    /// such ways is a graph-vertex/intersection candidate. Relation-member-only ways (`mask == 0`)
    /// never contribute here, matching how they never contribute to the extracted graph itself.
    pub use_counts: FxHashMap<i64, u32>,
    /// Node ids classified (selected) by a node topic — forced cut points even at use-count 1.
    pub selected: FxHashSet<i64>,
}

/// The topic-agnostic callbacks driving the "select" phase. `classify_way`/`classify_rel` return
/// the per-topic keep bitmask (`None`/`0` = kept by nobody); tag rows are emitted as a side effect
/// (see this module's own doc for why that can't wait for `SelectionContext`).
pub struct Callbacks<CR, CW, CN> {
    /// Whether any topic declares relation categories — gates the relations pass entirely.
    pub has_relations: bool,
    /// Relations pass: emit relation tag rows + `relation_members`; return the keep mask (`None`
    /// if kept by nobody). Fully independent of the ways pass for classification purposes —
    /// relation membership never affects whether a way is tag-kept.
    pub classify_rel: CR,
    /// OR of every topic index bit that wants *any* relation geometry (line/point/polygon) — i.e.
    /// `GeometryPlan::relation_geom_mask`. Gates which kept relations' member ways get their node
    /// refs/coords pulled in at all: a relation whose mask doesn't intersect this is tag-kept but
    /// nobody will ever build geometry for it, so its member ways would otherwise be indexed and
    /// have their coordinates decoded for nothing (this used to balloon Pass A/B cost whenever
    /// relations were enabled, regardless of how many relation topics were attribute-only).
    pub relation_geom_mask: u32,
    /// Ways pass: emit tag rows; return the keep mask (`None` if kept by nobody). Relation
    /// membership has no bearing on this — see `classify_and_index`'s own doc.
    pub classify_way: CW,
    /// Whether any topic declares node categories — gates node tag decoding in Pass B.
    pub has_nodes: bool,
    /// Nodes pass: emit node tag rows; return `true` if the node was selected (a forced cut
    /// point). A node's own point-geometry row (if wanted) is built and routed right here too, by
    /// the caller — a node is a leaf, its point shape needs nothing `SelectionContext` provides.
    pub classify_node: CN,
}

/// Assign a compact, sequential internal id to every graph vertex — a node that will appear as a
/// `start_id`/`end_id` in the `edges` table: shared between ≥2 ways, a way endpoint, or forced by a
/// node classifier (`selected`). Plain interior nodes of a single way never need an id since they're
/// never referenced by more than the one edge they're already embedded in.
///
/// Returns the `osm_id -> internal id` lookup and the `nodes` table rows (`internal id, osm_id, lon,
/// lat`). Called from `geom::materialize` (only when some topic wants the graph), not from here —
/// node-id assignment is a materialize-phase concern, not part of "select."
pub fn assign_node_ids(
    coords: &NodeCoords,
    endpoints: &FxHashSet<i64>,
    selected: &FxHashSet<i64>,
) -> (FxHashMap<i64, i64>, Vec<(i64, i64, f32, f32)>) {
    let mut ids: FxHashMap<i64, i64> = FxHashMap::default();
    let mut nodes: Vec<(i64, i64, f32, f32)> = Vec::new();
    let mut next_id: i64 = 1;
    for (&osm_id, &(lon, lat, shared)) in coords {
        if shared || endpoints.contains(&osm_id) || selected.contains(&osm_id) {
            let id = next_id;
            next_id += 1;
            ids.insert(osm_id, id);
            nodes.push((id, osm_id, lon, lat));
        }
    }
    (ids, nodes)
}

/// Run the "select" phase over an OSM PBF: classify relations, ways, and nodes (streaming tag rows
/// out via the `Callbacks`), and return a [`SelectionContext`] with everything needed to build
/// geometry afterward.
///
/// Fast path (sorted `node → way → relation` files) runs, in logical order:
///   * Relations pass — decode the relation region once: classify relations, emit relation rows,
///     collect `rel_members` for every kept one.
///   * Pass A — decode the way region once: classify ways purely by their own tags, tally per-node
///     use counts, index every kept way's (and every kept relation's member way's) node refs.
///   * Pass B — decode the node region once: collect coords for every referenced node and (if node
///     topics exist) classify node tags, collecting the selected-node set.
///
/// Fallback (unsorted / boundary check fails / `PBF_FORCE_FALLBACK`): full parallel scans.
pub fn stream_osm<CR, CW, CN>(path: &str, cb: Callbacks<CR, CW, CN>) -> anyhow::Result<SelectionContext>
where
    CR: for<'a> Fn(&RelData<'a>) -> Option<u32> + Sync + Send,
    CW: for<'a> Fn(&WayData<'a>) -> Option<u32> + Sync + Send,
    CN: for<'a> Fn(&NodeData<'a>) -> bool + Sync + Send,
{
    info!("Building blob index (no decompression)...");
    let t_idx = std::time::Instant::now();
    let (data_offsets, header_offset) = build_blob_index(path)?;
    info!("[phase] blob index build: {:.1}s", t_idx.elapsed().as_secs_f32());

    // One shared, read-only memory map for every blob decode below — avoids a fresh
    // open()+seek() syscall per blob (see `blob_index::decode_block`'s own doc).
    let file = std::fs::File::open(path).with_context(|| format!("opening PBF at {path}"))?;
    let mmap = unsafe { osmpbf::Mmap::from_file(&file)? };

    // Escape hatch: `PBF_FORCE_FALLBACK=1` skips the ordered fast path (debugging / a file that
    // wrongly advertises Sort.Type_then_ID).
    let force_fallback = std::env::var_os("PBF_FORCE_FALLBACK").is_some();
    let sorted = !force_fallback
        && header_offset
            .map(|off| pbf_is_sorted(path, off).unwrap_or(false))
            .unwrap_or(false);

    if sorted {
        info!(
            "PBF declares Sort.Type_then_ID — ordered streaming reader ({} data blobs)",
            data_offsets.len()
        );
        // The only "risky" steps (the sort-order assumptions) are the boundary searches; they run
        // before any streaming, so a failure can still fall back cleanly.
        match find_regions(&mmap, &data_offsets) {
            Ok((way_start, rel_start)) => {
                let node_offsets = &data_offsets[..way_start];
                let way_offsets = &data_offsets[way_start..rel_start];
                let rel_offsets = &data_offsets[rel_start..];
                info!(
                    "Regions — nodes: {} blobs, ways: {} blobs, relations: {} blobs",
                    node_offsets.len(),
                    way_offsets.len(),
                    rel_offsets.len()
                );

                // Relations pass — classify + emit relation rows, collect member-way requests.
                let rel_members = if cb.has_relations && !rel_offsets.is_empty() {
                    let t = std::time::Instant::now();
                    let m = classify_relations(&mmap, rel_offsets, &cb.classify_rel)?;
                    info!("[phase] Relations pass (classify + emit): {:.1}s ({} kept)", t.elapsed().as_secs_f32(), m.len());
                    m
                } else {
                    FxHashMap::default()
                };
                // Every relation-member way id needs its node refs recorded too (as a `mask == 0`
                // `way_refs` entry) even when its own tags never tag-keep it — see
                // `SelectionContext::way_refs`'s own doc. But only for relations whose mask
                // intersects `relation_geom_mask`: a relation kept purely for attribute output (no
                // topic wants its geometry) has nothing to gain from its member ways' coordinates.
                let extra_way_ids: FxHashSet<i64> = rel_members
                    .values()
                    .filter(|(_, mask)| mask & cb.relation_geom_mask != 0)
                    .flat_map(|(members, _)| members.iter().map(|&(w, _)| w))
                    .collect();

                // Pass A — way region (decoded once): classify + counts + endpoints + way_refs.
                let t = std::time::Instant::now();
                let (use_counts, endpoints, way_refs) =
                    classify_and_index(&mmap, way_offsets, &cb.classify_way, &extra_way_ids)?;
                info!("[phase] Pass A (classify ways + emit tags): {:.1}s", t.elapsed().as_secs_f32());

                // Pass B — node region: coords for every referenced node (+ classify nodes).
                let extra_node_ids: FxHashSet<i64> = way_refs
                    .iter()
                    .filter(|(_, (_, mask))| *mask == 0)
                    .flat_map(|(_, (refs, _))| refs.iter().copied())
                    .collect();
                let t = std::time::Instant::now();
                let (node_coords, selected, standalone_classified) = collect_coords(
                    &mmap, node_offsets, &use_counts, cb.has_nodes, &cb.classify_node, &extra_node_ids,
                )?;
                info!(
                    "[phase] Pass B (collect node coords{}): {:.1}s",
                    if cb.has_nodes { " + classify nodes" } else { "" },
                    t.elapsed().as_secs_f32()
                );
                resolve::log_node_summary(&use_counts, standalone_classified);
                let _ = endpoints; // endpoints are re-derived from way_refs by geom::materialize (mask != 0 subset)

                return Ok(SelectionContext { node_coords, way_refs, rel_members, use_counts, selected });
            }
            Err(e) => {
                warn!("ordered fast-path boundary check failed ({e:#}); falling back to full scan");
            }
        }
    } else {
        warn!("PBF not declared Sort.Type_then_ID — using full-scan streaming reader");
    }

    stream_osm_fallback(path, cb)
}

/// Locate the `(way_start, rel_start)` region boundaries in a sorted file's blob list.
fn find_regions(mmap: &osmpbf::Mmap, data: &[osmpbf::ByteOffset]) -> anyhow::Result<(usize, usize)> {
    let way_start = find_way_section_start(mmap, data)?;
    let rel_start = find_relation_section_start(mmap, data, way_start)?;
    Ok((way_start, rel_start))
}
