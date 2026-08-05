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
use super::resolve::{dense_node_data, node_data, rel_data, way_data, NodeCoords, NodeCoordsBuilder};
use super::way_refs::EncodedRefs;

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

/// Pass A — decode the way-region blobs once (parallel). For every way, extract its `WayData` and
/// classify it (side effect: emit tag rows); a way is *kept* (mask != 0) when `classify` returns
/// `Some` — purely its own tag classification, independent of relation membership. Every kept way,
/// plus every way in `extra_way_ids` (a relation-member way that might not be tag-kept itself —
/// see `SelectionContext::way_refs`'s own doc), gets a `way_refs` entry; a relation-only way's
/// entry carries mask `0`. Only mask-!=0 ways feed `use_counts`/`endpoints` (the main graph's
/// intersection-detection inputs) — a relation-only way never affects them, matching how it never
/// contributes to the extracted graph itself.
pub(super) fn classify_and_index<C>(
    mmap: &osmpbf::Mmap,
    way_offsets: &[ByteOffset],
    classify: &C,
    extra_way_ids: &FxHashSet<i64>,
) -> anyhow::Result<(FxHashMap<i64, u32>, FxHashSet<i64>, FxHashMap<i64, (EncodedRefs, u32)>)>
where
    C: for<'a> Fn(&WayData<'a>) -> Option<u32> + Sync,
{
    way_offsets
        .par_iter()
        .map(|&off| -> anyhow::Result<(FxHashMap<i64, u32>, FxHashSet<i64>, FxHashMap<i64, (EncodedRefs, u32)>)> {
            let block = decode_block(mmap, off)?;
            let mut counts: FxHashMap<i64, u32> = FxHashMap::default();
            let mut endpoints: FxHashSet<i64> = FxHashSet::default();
            let mut way_refs: FxHashMap<i64, (EncodedRefs, u32)> = FxHashMap::default();
            for group in block.groups() {
                for way in group.ways() {
                    let wd = way_data(&way);
                    // `classify` has a side effect (emits tag rows) — called exactly once per way.
                    let kept_mask = classify(&wd);
                    let is_extra = !extra_way_ids.is_empty() && extra_way_ids.contains(&wd.id);
                    if kept_mask.is_none() && !is_extra {
                        continue;
                    }
                    // Deltas straight off the wire (see `way_data`'s own doc) — accumulate to
                    // absolute ids ourselves only where one's actually needed (counts/endpoints),
                    // and hand the deltas straight to `EncodedRefs` untouched otherwise.
                    let deltas = way.raw_refs();
                    if kept_mask.is_some() {
                        let mut cur = 0i64;
                        let mut first = None;
                        for &delta in deltas {
                            cur += delta;
                            first.get_or_insert(cur);
                            *counts.entry(cur).or_insert(0) += 1;
                        }
                        if let Some(first) = first {
                            endpoints.insert(first);
                            endpoints.insert(cur);
                        }
                    }
                    way_refs.insert(wd.id, (EncodedRefs::from_deltas(deltas), kept_mask.unwrap_or(0)));
                }
            }
            Ok((counts, endpoints, way_refs))
        })
        .try_reduce(
            || (FxHashMap::default(), FxHashSet::default(), FxHashMap::default()),
            |a, b| {
                let (mut ca, mut ea, mut ra) = a;
                let (cb, eb, rb) = b;
                for (k, v) in cb {
                    *ca.entry(k).or_insert(0) += v;
                }
                ea.extend(eb);
                ra.extend(rb);
                Ok((ca, ea, ra))
            },
        )
}

/// How many node blobs Pass B decodes in parallel before folding their output into the coordinate
/// map. This bounds the *transient* cost of the decoded-but-not-yet-folded `Vec`s: collecting every
/// blob first (which is what this used to do) meant the whole file's node data was resident twice at
/// the moment the fold began — once as 24-byte `Vec` tuples, once in the growing map. At ~8k nodes
/// per blob this caps that transient at a few tens of MB regardless of file size.
///
/// The fold stays sequential and *in blob order* on purpose. A producer/consumer channel would
/// overlap decoding with folding and be faster, but blobs would arrive in completion order, which
/// changes the insertion order into the map. `FxHashMap` is unseeded, so its iteration order is a
/// deterministic function of insertion order, and `assign_node_ids` hands out internal graph-vertex
/// ids in exactly that order — reordering insertions would silently renumber every row of the
/// `nodes` and `edges` tables. Not worth it for a fold that is a small fraction of decode time.
const FOLD_CHUNK_BLOBS: usize = 256;

/// Pass B — collect coordinates (as f32, ~1 m precision) for the needed nodes from the node-region
/// blobs, in parallel (in chunks of `FOLD_CHUNK_BLOBS`, see its doc). Each node's `shared` flag
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
    use_counts: &FxHashMap<i64, u32>,
    classify_nodes: bool,
    classify_node: &CN,
    extra_node_ids: &FxHashSet<i64>,
    disk_node_store: bool,
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
    let mut coords = NodeCoordsBuilder::with_capacity(disk_node_store, distinct_needed);
    let mut selected: FxHashSet<i64> = FxHashSet::default();
    let mut standalone_total: u64 = 0;

    for blob_chunk in node_offsets.chunks(FOLD_CHUNK_BLOBS) {
        let per_blob: Vec<(Vec<(i64, f32, f32, bool)>, FxHashSet<i64>, u64)> = blob_chunk
            .par_iter()
            .map(|&off| -> anyhow::Result<(Vec<(i64, f32, f32, bool)>, FxHashSet<i64>, u64)> {
                let block = decode_block(mmap, off)?;
                let mut out = Vec::new();
                let mut selected: FxHashSet<i64> = FxHashSet::default();
                let mut standalone: u64 = 0;
                for group in block.groups() {
                    for n in group.dense_nodes() {
                        if let Some(&c) = use_counts.get(&n.id()) {
                            out.push((n.id(), n.lon() as f32, n.lat() as f32, c > 1));
                            if classify_nodes && classify_node(&dense_node_data(&n)) {
                                selected.insert(n.id());
                            }
                        } else if extra_node_ids.contains(&n.id()) {
                            out.push((n.id(), n.lon() as f32, n.lat() as f32, false));
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
                        if let Some(&c) = use_counts.get(&n.id()) {
                            out.push((n.id(), n.lon() as f32, n.lat() as f32, c > 1));
                            if classify_nodes && classify_node(&node_data(&n)) {
                                selected.insert(n.id());
                            }
                        } else if extra_node_ids.contains(&n.id()) {
                            out.push((n.id(), n.lon() as f32, n.lat() as f32, false));
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
    Ok((coords.finish()?, selected, standalone_total))
}
