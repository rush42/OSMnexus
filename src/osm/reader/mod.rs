//! Topic-agnostic PBF streaming reader — the "select" phase. Drives tag classification of
//! relations, ways, and nodes over a PBF (the sorted fast path decodes each blob region once, in
//! the logical order relations → ways → nodes; an unsorted file falls back to full scans), and
//! returns a [`SelectionContext`]: everything geometry construction needs (node coordinates with
//! their graph-vertex `shared` flag already folded in, each kept way's node refs, each kept
//! relation's member ways) — but no geometry itself. That's `geom::materialize`'s job, run
//! separately afterward over the returned context; this module never builds or routes a geometry
//! row. Anything the passes need only among themselves — the raw per-node use counts, the endpoint
//! set — is dropped before returning rather than carried through the materialize phase.
//!
//! Tag-row output is the one thing that *can't* wait for a returned value — buffering every tag
//! row in memory until the whole file is classified is exactly what an earlier version of this
//! pipeline did and OOM'd on a country-sized import (see the project history). So `classify_rel`/
//! `classify_way`/`classify_node` still stream their tag rows out as a side effect while the file
//! is read; only the (much smaller — bounded by kept elements' id/node-ref data, not tag content)
//! geometry inputs are buffered and returned.

mod blob_index;
mod fallback;
mod memory_coords;
mod rel_members;
mod resolve;
mod sorted;
mod store;
mod way_refs;

use anyhow::Context;
use rustc_hash::{FxHashMap, FxHashSet};
use tracing::{info, warn};

use crate::geom::rows::PointRow;
use crate::osm::types::{NodeData, RelData, WayData};
use crate::output::rows::{MemberRow, TopicRow};

use blob_index::{build_blob_index, find_relation_section_start, find_way_section_start, pbf_is_sorted};
use fallback::stream_osm_fallback;
pub use rel_members::RelMembers;
pub use resolve::{resolve_geometry, NodeCoords};
use sorted::{classify_and_index, classify_relations, collect_coords};
pub use way_refs::{EncodedRefs, WayRefsStore};

/// Everything geometry construction needs, once the "select" phase (relations → ways → nodes)
/// finishes — see this module's own doc for why it's returned as one value instead of driven via
/// per-element geometry callbacks the way tag classification still is.
pub struct SelectionContext {
    /// Every node referenced by a `way_refs` entry (kept way or relation-member way alike).
    pub node_coords: NodeCoords,
    /// Every kept/relation-member way's (delta+varint-encoded node refs, per-topic keep mask) — see
    /// `way_refs`'s own module doc for why the refs aren't a plain `Vec<i64>`. `mask == 0` means the
    /// way was never tag-kept by any topic — it's here purely because some kept relation in
    /// `rel_members` references it, so its own shapes (line/point/polygon/graph) are never built,
    /// only its coordinates are available for relation-geometry assembly.
    pub way_refs: WayRefsStore,
    /// `relation_id -> (member ways with role, per-topic keep mask)`, for every tag-kept relation
    /// (regardless of whether any topic wants relation geometry — that decision is
    /// `geom::materialize`'s, using a `GeometryPlan`, not this module's).
    pub rel_members: RelMembers,
    /// Node ids classified by a node topic that also declared `"geometry": {"node": ["graph"]}` —
    /// forced cut points even at use-count 1. A node classified only by point-only (or bare) node
    /// topics is not in here — see `GeometryPlan::node_graph_mask`.
    pub selected: FxHashSet<i64>,
}

/// The topic-agnostic callbacks driving the "select" phase. `classify_way`/`classify_rel`/
/// `classify_node` are now pure — they return their tag rows (and, for a way/relation, the keep
/// mask) instead of routing them as a side effect. Routing is a separate `route_*` callback, called
/// by the ordered fast path from its blob-order sequential fold (not from the parallel decode
/// section the classify closures run in) — see `sorted::FOLD_CHUNK_BLOBS`'s own doc for why that
/// fold is already blob-ordered, and `classify_and_index`'s own doc for why tag routing used to
/// race ahead of it. The fallback scan (`fallback::stream_osm_fallback`) calls `route_*` right next
/// to `classify_*` instead, same as before — it has no ordering guarantee to preserve.
pub struct Callbacks<CR, CW, CN, RT, RM, RP> {
    /// Whether any topic declares relation categories — gates the relations pass entirely.
    pub has_relations: bool,
    /// Relations pass: classify one relation, returning its keep mask (`None` if kept by nobody),
    /// its per-topic tag rows, and its member-way link rows (empty unless kept). Pure — no routing.
    /// Fully independent of the ways pass for classification purposes — relation membership never
    /// affects whether a way is tag-kept.
    pub classify_rel: CR,
    /// OR of every topic index bit that wants *any* relation geometry (line/point/polygon) — i.e.
    /// `GeometryPlan::relation_geom_mask`. Gates which kept relations' member ways get their node
    /// refs/coords pulled in at all: a relation whose mask doesn't intersect this is tag-kept but
    /// nobody will ever build geometry for it, so its member ways would otherwise be indexed and
    /// have their coordinates decoded for nothing (this used to balloon Pass A/B cost whenever
    /// relations were enabled, regardless of how many relation topics were attribute-only).
    pub relation_geom_mask: u32,
    /// Ways pass: classify one way, returning its keep mask (`None` if kept by nobody) and its
    /// per-topic tag rows. Pure — no routing. Relation membership has no bearing on this — see
    /// `classify_and_index`'s own doc.
    pub classify_way: CW,
    /// Whether any topic declares node categories — gates node tag decoding in Pass B.
    pub has_nodes: bool,
    /// Nodes pass: classify one node, returning whether it should be a forced graph cut point
    /// (classified by a topic that declared `"geometry": {"node": ["graph"]}` — not merely
    /// classified at all, see `GeometryPlan::node_graph_mask`), its per-topic tag rows, and (only
    /// when kept) its own point-geometry row paired with the keep mask — a node is a leaf, its
    /// point shape needs nothing `SelectionContext` provides. Pure — no routing.
    pub classify_node: CN,
    /// Every node-processing topic provably yields nothing for a node with no tags, so untagged
    /// nodes can be skipped without decoding them (see `TopicRunner::skips_untagged`). The great
    /// majority of nodes in an OSM extract carry no tags at all — they exist only as way geometry —
    /// so on a category-based node topic this elides nearly the whole per-node cost (tag decode,
    /// `NodeData` construction, classification) that Pass B would otherwise pay to reject them.
    ///
    /// False whenever any topic *could* match an untagged node — `accept_all`, or a filter that's
    /// satisfied by absence (e.g. "`highway` not present") — in which case every node is decoded as
    /// before.
    pub skip_untagged_nodes: bool,
    /// `plan.any_way_graph` — whether any topic wants way graph output. Pass A's `use_counts` only
    /// needs actual per-node reference *counts* (not just membership) to derive the `shared`
    /// cut-point flag, and that flag is only ever read when this is set (`geom::materialize::run`
    /// gates `assign_node_ids`/cut-point logic on the same flag) — see `NodeRefCounts`'s own doc
    /// for the cheaper shape Pass A builds instead when this is `false`.
    pub needs_graph: bool,
    /// Route a classified element's per-topic tag rows to the tag-table writers. Called from the
    /// ordered fast path's sequential, blob-order fold — see this struct's own doc.
    pub route_tag: RT,
    /// Route a kept relation's member-way link rows.
    pub route_member: RM,
    /// Route a kept node's own point-geometry row (mask, row).
    pub route_point: RP,
}

/// Log what the reader's own long-lived structures occupy at a phase boundary, next to the
/// process's current anonymous RSS.
///
/// The point is the *residual*: total RSS minus these structures. A small residual means the
/// pipeline's memory is understood and accounted for; a large one means something unmodelled is
/// resident — a structure not listed here, or memory the allocator is holding rather than returning
/// to the OS. Peak-RSS numbers alone can't distinguish those, and a heap profiler can't see the
/// second case at all (it counts requested bytes, not what glibc keeps in its arenas).
///
/// Individual sizes are lower bounds where noted (MPHFs aren't measurable through `boomphf`'s API).
/// Anonymous RSS specifically, not total: the PBF is mmap'd, and its resident pages would otherwise
/// swamp the comparison — see this reader's benchmarking notes.
fn log_struct_sizes(phase: &str, parts: &[(&str, usize)]) {
    const MB: f64 = 1024.0 * 1024.0;
    let total: usize = parts.iter().map(|&(_, b)| b).sum();
    let detail: Vec<String> =
        parts.iter().map(|&(name, b)| format!("{name}={:.0}MB", b as f64 / MB)).collect();
    let rss_anon = read_rss_anon_bytes();
    match rss_anon {
        Some(rss) => info!(
            "[mem] {phase}: {} | accounted={:.0}MB RssAnon={:.0}MB residual={:.0}MB",
            detail.join(" "),
            total as f64 / MB,
            rss as f64 / MB,
            (rss.saturating_sub(total)) as f64 / MB,
        ),
        None => info!("[mem] {phase}: {} | accounted={:.0}MB", detail.join(" "), total as f64 / MB),
    }
}

/// Current anonymous resident bytes from `/proc/self/status` (Linux only; `None` elsewhere or if
/// the field is missing). `RssAnon` rather than `VmRSS` deliberately — see `log_struct_sizes`.
fn read_rss_anon_bytes() -> Option<usize> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let line = status.lines().find(|l| l.starts_with("RssAnon:"))?;
    let kb: usize = line.split_whitespace().nth(1)?.parse().ok()?;
    Some(kb * 1024)
}

/// Assign a compact, sequential internal id to every graph vertex — a node that will appear as a
/// `start_id`/`end_id` in the `edges` table: shared between ≥2 ways, a way endpoint, or forced by a
/// node topic wanting graph output (`selected`). Plain interior nodes of a single way never need an
/// id since they're never referenced by more than the one edge they're already embedded in.
///
/// Returns the `osm_id -> internal id` lookup and the `nodes` table rows (`internal id, osm_id, lon,
/// lat`). Called from `geom::materialize` (only when some topic wants the graph), not from here —
/// node-id assignment is a materialize-phase concern, not part of "select."
pub fn assign_node_ids(
    coords: &NodeCoords,
    endpoints: &FxHashSet<i64>,
    selected: &FxHashSet<i64>,
) -> (FxHashMap<i64, i64>, Vec<(i64, i64, f64, f64)>) {
    let mut ids: FxHashMap<i64, i64> = FxHashMap::default();
    let mut nodes: Vec<(i64, i64, f64, f64)> = Vec::new();
    let mut next_id: i64 = 1;
    for (osm_id, (lon, lat, shared)) in coords.iter() {
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
pub fn stream_osm<CR, CW, CN, RT, RM, RP>(
    path: &str,
    cb: Callbacks<CR, CW, CN, RT, RM, RP>,
) -> anyhow::Result<SelectionContext>
where
    CR: for<'a> Fn(&RelData<'a>) -> (Option<u32>, Vec<Vec<TopicRow>>, Vec<MemberRow>) + Sync + Send,
    CW: for<'a> Fn(&WayData<'a>) -> (Option<u32>, Vec<Vec<TopicRow>>) + Sync + Send,
    CN: for<'a> Fn(&NodeData<'a>) -> (bool, Vec<Vec<TopicRow>>, Option<(u32, PointRow)>) + Sync + Send,
    RT: Fn(Vec<Vec<TopicRow>>) + Sync + Send,
    RM: Fn(Vec<MemberRow>) + Sync + Send,
    RP: Fn(u32, PointRow) + Sync + Send,
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
                    let m = classify_relations(&mmap, rel_offsets, &cb.classify_rel, &cb.route_tag, &cb.route_member)?;
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

                // Pass A — way region (decoded once): classify + counts + way_refs. `extra_node_ids`
                // (mask-`0` relation-only ways' own node ids, needed for relation-geometry assembly)
                // comes back straight from the fold instead of a second scan over the finished
                // `way_refs` — see `classify_and_index`'s own doc.
                let t = std::time::Instant::now();
                let (use_counts, way_refs, extra_node_ids) = classify_and_index(
                    &mmap, way_offsets, &cb.classify_way, &cb.route_tag, &extra_way_ids, cb.needs_graph,
                )?;
                info!("[phase] Pass A (classify ways + emit tags): {:.1}s", t.elapsed().as_secs_f32());
                log_struct_sizes(
                    "after Pass A",
                    &[
                        ("use_counts", use_counts.heap_bytes()),
                        ("way_refs", way_refs.heap_bytes()),
                        ("extra_node_ids", extra_node_ids.capacity() * (std::mem::size_of::<i64>() + 1)),
                    ],
                );

                // Pass B — node region: coords for every referenced node (+ classify nodes).
                let t = std::time::Instant::now();
                let (node_coords, selected, standalone_classified) = collect_coords(
                    &mmap, node_offsets, &use_counts, cb.has_nodes, &cb.classify_node, &cb.route_tag,
                    &cb.route_point, &extra_node_ids, cb.skip_untagged_nodes,
                )?;
                info!(
                    "[phase] Pass B (collect node coords{}): {:.1}s",
                    if cb.has_nodes { " + classify nodes" } else { "" },
                    t.elapsed().as_secs_f32()
                );
                resolve::log_node_summary(&use_counts, standalone_classified);
                log_struct_sizes(
                    "after Pass B",
                    &[
                        ("node_coords", node_coords.heap_bytes()),
                        ("use_counts", use_counts.heap_bytes()),
                        ("way_refs", way_refs.heap_bytes()),
                        ("extra_node_ids", extra_node_ids.capacity() * (std::mem::size_of::<i64>() + 1)),
                        ("selected", selected.capacity() * (std::mem::size_of::<i64>() + 1)),
                    ],
                );

                // `use_counts` has no consumer past this point — the `shared` flag it feeds is
                // already baked into `node_coords` (see `NodeCoords`' own doc). Dropped explicitly,
                // and before the return, because it's one entry per referenced node: carrying it
                // into the materialize phase (which is what returning it in `SelectionContext` used
                // to do) costs the same order of memory as the coordinate map itself, for nothing.
                drop(use_counts);

                return Ok(SelectionContext { node_coords, way_refs, rel_members: RelMembers::build(rel_members), selected });
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
