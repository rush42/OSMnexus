//! The sorted fast path: the relations pass (classify + emit, collect member-way requests), Pass A
//! (classify ways + index, plus a side channel for any `extra_way_ids`), and Pass B (collect node
//! coords + classify nodes, widened for `extra_way_ids`' node ids) — each decoding its blob region
//! once. No geometry pass here — resolving/building geometry is `geom::materialize`'s job, run
//! separately over the `SelectionContext` these passes produce.

use rustc_hash::{FxHashMap, FxHashSet};
use osmpbf::ByteOffset;
use rayon::prelude::*;

use crate::osm::types::{MemberRole, NodeData, RelData, WayData};

use super::blob_index::decode_block;
use super::resolve::{dense_node_data, node_data, rel_data, way_data, NodeCoords, NodeCoordsBuilder, NodeRefCounts};
use super::way_refs::{EncodedRefs, WayRefsStore};

/// Relations pass — decode the relation region once (parallel). For every relation, extract its
/// `RelData` and run `classify_rel` (side effect: emit relation tag rows + `relation_members`
/// links); when it returns a keep mask, record `(relation_id, (member ways with role, mask))` —
/// consumed both to build `SelectionContext::rel_members` and (by the caller, `stream_osm`) to
/// derive which ways need their node refs recorded regardless of their own tag-keep status.
pub(super) fn classify_relations<CR>(
    mmap: &osmpbf::Mmap,
    rel_offsets: &[ByteOffset],
    classify_rel: &CR,
) -> anyhow::Result<FxHashMap<i64, (Vec<(i64, MemberRole)>, u32)>>
where
    CR: for<'a> Fn(&RelData<'a>) -> Option<u32> + Sync,
{
    rel_offsets
        .par_iter()
        .map(|&off| -> anyhow::Result<FxHashMap<i64, (Vec<(i64, MemberRole)>, u32)>> {
            let block = decode_block(mmap, off)?;
            let mut out = FxHashMap::default();
            for group in block.groups() {
                for rel in group.relations() {
                    let rd = rel_data(&rel);
                    if let Some(mask) = classify_rel(&rd) {
                        out.insert(rd.id, (rd.member_ways, mask));
                    }
                }
            }
            Ok(out)
        })
        .try_reduce(FxHashMap::default, |mut a, b| {
            a.extend(b);
            Ok(a)
        })
}

/// How many blobs a pass decodes in parallel before folding their output sequentially into the
/// pass's accumulators. This bounds the *transient* cost of the decoded-but-not-yet-folded `Vec`s:
/// collecting every blob first (which is what Pass B used to do) meant the whole file's data was
/// resident twice at the moment the fold began — once as per-record `Vec` tuples, once in the
/// growing accumulator. At ~8k records per blob this caps that transient at a few tens of MB
/// regardless of file size. Shared by Pass A (`classify_and_index`) and Pass B (`collect_coords`).
///
/// The fold stays sequential and *in blob order* on purpose. A producer/consumer channel would
/// overlap decoding with folding and be faster, but blobs would arrive in completion order, which
/// changes the insertion order into the accumulator. `FxHashMap`/`FxHashSet` are unseeded, so their
/// iteration order is a deterministic function of insertion order, and `assign_node_ids` hands out
/// internal graph-vertex ids in exactly that order — reordering insertions would silently renumber
/// every row of the `nodes` and `edges` tables. Not worth it for a fold that is a small fraction of
/// decode time.
const FOLD_CHUNK_BLOBS: usize = 256;

/// One classified way, ready to fold into Pass A's accumulators: its id, encoded node refs, and
/// per-topic keep mask, plus (only when tag-kept — `mask != 0`) its absolute node ids, needed to
/// update `use_counts`/`present`. A relation-only way (`mask == 0`) carries `None` here — its own
/// node refs never feed the main graph's intersection detection, only relation-geometry assembly
/// (via `extra_node_ids`, derived from `refs` itself during the fold instead).
struct ClassifiedWay {
    id: i64,
    refs: EncodedRefs,
    mask: u32,
    node_ids: Option<Vec<i64>>,
}

/// Pass A — decode the way-region blobs once, in bounded parallel chunks (`FOLD_CHUNK_BLOBS`, same
/// pattern as Pass B's `collect_coords`), folding each chunk's ways sequentially into three
/// accumulators. For every way, extract its `WayData` and classify it (side effect: emit tag rows);
/// a way is *kept* (mask != 0) when `classify` returns `Some` — purely its own tag classification,
/// independent of relation membership. Every kept way, plus every way in `extra_way_ids` (a
/// relation-member way that might not be tag-kept itself — see `SelectionContext::way_refs`'s own
/// doc), gets a `way_refs` entry; a relation-only way's entry carries mask `0`. Only mask-!=0 ways
/// feed `use_counts` (the main graph's intersection-detection input) — a relation-only way never
/// affects it, matching how it never contributes to the extracted graph itself. A relation-only
/// way's own node ids are folded into the returned `extra_node_ids` instead (mirrors what
/// `mod.rs` used to derive from the finished `way_refs` after the fact — cheaper to collect during
/// the one pass that already decodes these ways than to re-scan the finished store for it).
///
/// `needs_graph` (`plan.any_way_graph` at the call site) picks `use_counts`' shape: `Counted`
/// (tracks each node's reference count, to derive the `shared` cut-point flag) when some topic
/// wants graph output, `Present` (membership only — cheaper, no per-node counting) when none do —
/// see `NodeRefCounts`'s own doc. Doesn't build `endpoints` at all: `geom::materialize` derives its
/// own from `way_refs` when it actually needs them (`plan.any_way_graph`), so a Pass-A copy was
/// always dead weight.
pub(super) fn classify_and_index<C>(
    mmap: &osmpbf::Mmap,
    way_offsets: &[ByteOffset],
    classify: &C,
    extra_way_ids: &FxHashSet<i64>,
    needs_graph: bool,
) -> anyhow::Result<(NodeRefCounts, WayRefsStore, FxHashSet<i64>)>
where
    C: for<'a> Fn(&WayData<'a>) -> Option<u32> + Sync,
{
    let mut counts: FxHashMap<i64, u32> = FxHashMap::default();
    let mut present: FxHashSet<i64> = FxHashSet::default();
    let mut way_refs: FxHashMap<i64, (EncodedRefs, u32)> = FxHashMap::default();
    let mut extra_node_ids: FxHashSet<i64> = FxHashSet::default();

    for blob_chunk in way_offsets.chunks(FOLD_CHUNK_BLOBS) {
        let per_blob: Vec<Vec<ClassifiedWay>> = blob_chunk
            .par_iter()
            .map(|&off| -> anyhow::Result<Vec<ClassifiedWay>> {
                let block = decode_block(mmap, off)?;
                let mut out = Vec::new();
                for group in block.groups() {
                    for way in group.ways() {
                        let wd = way_data(&way);
                        let kept_mask = classify(&wd);
                        let is_extra = !extra_way_ids.is_empty() && extra_way_ids.contains(&wd.id);
                        if kept_mask.is_none() && !is_extra {
                            continue;
                        }
                        let deltas = way.raw_refs();
                        let node_ids = kept_mask.is_some().then(|| {
                            let mut cur = 0i64;
                            deltas
                                .iter()
                                .map(|&delta| {
                                    cur += delta;
                                    cur
                                })
                                .collect::<Vec<i64>>()
                        });
                        out.push(ClassifiedWay {
                            id: wd.id,
                            refs: EncodedRefs::from_deltas(deltas),
                            mask: kept_mask.unwrap_or(0),
                            node_ids,
                        });
                    }
                }
                Ok(out)
            })
            .collect::<anyhow::Result<Vec<_>>>()?;

        for ways in per_blob {
            for w in ways {
                match &w.node_ids {
                    Some(ids) => {
                        for &nid in ids {
                            if needs_graph {
                                *counts.entry(nid).or_insert(0) += 1;
                            } else {
                                present.insert(nid);
                            }
                        }
                    }
                    None => {
                        // Relation-only (`mask == 0`) — its own refs never touch the main graph's
                        // intersection detection, but its nodes still need coordinates for
                        // relation-geometry assembly (see `mod.rs`'s `extra_node_ids` use).
                        extra_node_ids.extend(w.refs.iter());
                    }
                }
                way_refs.insert(w.id, (w.refs, w.mask));
            }
        }
    }

    let use_counts =
        if needs_graph { NodeRefCounts::Counted(counts) } else { NodeRefCounts::Present(present) };
    Ok((use_counts, WayRefsStore::build(way_refs), extra_node_ids))
}

/// Pass B — collect coordinates (as fixed-point decimicrodegrees, the PBF's own exact integer form
/// — see `NodeCoords`) for the needed nodes from the node-region blobs, in parallel (in chunks of
/// `FOLD_CHUNK_BLOBS`, see its doc). Each node's `shared` flag
/// (used by ≥2 mask-!=0 ways) is read from `use_counts` here and baked into the value.
///
/// When `classify_nodes` is set, every needed node's tags are also decoded and passed to
/// `classify_node` (side effect: emit node tag rows); a node it returns `true` for is *selected*
/// and its id is accumulated into the returned set (forced graph-vertex cut points). When unset,
/// node tags are not decoded at all — preserving the way-only performance profile.
///
/// `extra_node_ids` widens which nodes get a coordinate fetched (mask-`0` `way_refs`' node ids),
/// but never affects `shared` (always computed from `use_counts` alone, `false` for an extra-only
/// node) or node-topic classification (only run for `use_counts` members) — pure coordinate
/// lookup, no side effects on the main graph's intersection/cut-point logic.
pub(super) fn collect_coords<CN>(
    mmap: &osmpbf::Mmap,
    node_offsets: &[ByteOffset],
    use_counts: &NodeRefCounts,
    classify_nodes: bool,
    classify_node: &CN,
    extra_node_ids: &FxHashSet<i64>,
) -> anyhow::Result<(NodeCoords, FxHashSet<i64>, u64)>
where
    CN: for<'a> Fn(&NodeData<'a>) -> bool + Sync,
{
    // Size the map up front. Growing from empty means the final doubling holds the old and new
    // bucket arrays at once — a transient of ~1.5x the final table, which at this cardinality is
    // GBs, and it turned out to be the single largest contributor to Pass B's peak (brandenburg
    // 1080 -> 976 MB peak RSS from this line alone).
    //
    // The hint is the exact count of *distinct* node ids that could be inserted. It's an upper
    // bound rather than the truth only because a referenced node may be absent from the file —
    // negligible for a complete download, but real for a clipped extract. That matters because
    // hashbrown rounds to `next_pow2(hint * 8/7)` buckets: overshooting by enough to cross a
    // power-of-two boundary would *double* the table permanently instead of saving the transient.
    // Checked against an `osmium extract -s simple` extract (dangling refs by construction, no
    // `complete_ways`) — still a win there, 158 -> 152 MB, so the boundary case isn't being hit in
    // practice. Re-measure that case before loosening the hint.
    let distinct_needed = use_counts.len()
        + extra_node_ids.iter().filter(|id| !use_counts.contains_key(id)).count();
    let mut coords = NodeCoordsBuilder::with_capacity(distinct_needed);
    let mut selected: FxHashSet<i64> = FxHashSet::default();
    let mut standalone_total: u64 = 0;

    for blob_chunk in node_offsets.chunks(FOLD_CHUNK_BLOBS) {
        let per_blob: Vec<(Vec<(i64, i32, i32, bool)>, FxHashSet<i64>, u64)> = blob_chunk
            .par_iter()
            .map(|&off| -> anyhow::Result<(Vec<(i64, i32, i32, bool)>, FxHashSet<i64>, u64)> {
                let block = decode_block(mmap, off)?;
                let mut out = Vec::new();
                let mut selected: FxHashSet<i64> = FxHashSet::default();
                let mut standalone: u64 = 0;
                for group in block.groups() {
                    for n in group.dense_nodes() {
                        if let Some(shared) = use_counts.lookup(&n.id()) {
                            out.push((n.id(), n.decimicro_lon(), n.decimicro_lat(), shared));
                            if classify_nodes && classify_node(&dense_node_data(&n)) {
                                selected.insert(n.id());
                            }
                        } else if extra_node_ids.contains(&n.id()) {
                            out.push((n.id(), n.decimicro_lon(), n.decimicro_lat(), false));
                        } else if classify_nodes {
                            // Not part of any kept way — still classify it (tag rows / point
                            // geometry are driven by `classify_node` itself from `NodeData`, not
                            // `NodeCoords`), just don't hold its coords or count it toward graph
                            // cut points.
                            classify_node(&dense_node_data(&n));
                            standalone += 1;
                        }
                    }
                    for n in group.nodes() {
                        if let Some(shared) = use_counts.lookup(&n.id()) {
                            out.push((n.id(), n.decimicro_lon(), n.decimicro_lat(), shared));
                            if classify_nodes && classify_node(&node_data(&n)) {
                                selected.insert(n.id());
                            }
                        } else if extra_node_ids.contains(&n.id()) {
                            out.push((n.id(), n.decimicro_lon(), n.decimicro_lat(), false));
                        } else if classify_nodes {
                            classify_node(&node_data(&n));
                            standalone += 1;
                        }
                    }
                }
                Ok((out, selected, standalone))
            })
            .collect::<anyhow::Result<Vec<_>>>()?;

        for (chunk, sel, standalone) in per_blob {
            for (id, lon, lat, shared) in chunk {
                coords.insert(id, lon, lat, shared);
            }
            selected.extend(sel);
            standalone_total += standalone;
        }
    }
    Ok((coords.finish(), selected, standalone_total))
}
