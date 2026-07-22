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
use super::resolve::{dense_node_data, node_data, rel_data, way_data, NodeCoords};

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
) -> anyhow::Result<(FxHashMap<i64, u32>, FxHashSet<i64>, FxHashMap<i64, (Vec<i64>, u32)>)>
where
    C: for<'a> Fn(&WayData<'a>) -> Option<u32> + Sync,
{
    way_offsets
        .par_iter()
        .map(|&off| -> anyhow::Result<(FxHashMap<i64, u32>, FxHashSet<i64>, FxHashMap<i64, (Vec<i64>, u32)>)> {
            let block = decode_block(mmap, off)?;
            let mut counts: FxHashMap<i64, u32> = FxHashMap::default();
            let mut endpoints: FxHashSet<i64> = FxHashSet::default();
            let mut way_refs: FxHashMap<i64, (Vec<i64>, u32)> = FxHashMap::default();
            for group in block.groups() {
                for way in group.ways() {
                    let wd = way_data(&way);
                    // `classify` has a side effect (emits tag rows) — called exactly once per way.
                    let kept_mask = classify(&wd);
                    let is_extra = !extra_way_ids.is_empty() && extra_way_ids.contains(&wd.id);
                    if kept_mask.is_none() && !is_extra {
                        continue;
                    }
                    if kept_mask.is_some() {
                        for &id in &wd.node_refs {
                            *counts.entry(id).or_insert(0) += 1;
                        }
                        if let (Some(&first), Some(&last)) = (wd.node_refs.first(), wd.node_refs.last()) {
                            endpoints.insert(first);
                            endpoints.insert(last);
                        }
                    }
                    way_refs.insert(wd.id, (wd.node_refs, kept_mask.unwrap_or(0)));
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

/// Pass B — collect coordinates (as f32, ~1 m precision) for the needed nodes from the node-region
/// blobs, in parallel. Each node's `shared` flag (used by ≥2 mask-!=0 ways) is read from
/// `use_counts` here and baked into the value.
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
) -> anyhow::Result<(NodeCoords, FxHashSet<i64>)>
where
    CN: for<'a> Fn(&NodeData<'a>) -> bool + Sync,
{
    let per_blob: Vec<(Vec<(i64, f32, f32, bool)>, FxHashSet<i64>)> = node_offsets
        .par_iter()
        .map(|&off| -> anyhow::Result<(Vec<(i64, f32, f32, bool)>, FxHashSet<i64>)> {
            let block = decode_block(mmap, off)?;
            let mut out = Vec::new();
            let mut selected: FxHashSet<i64> = FxHashSet::default();
            for group in block.groups() {
                for n in group.dense_nodes() {
                    if let Some(&c) = use_counts.get(&n.id()) {
                        out.push((n.id(), n.lon() as f32, n.lat() as f32, c > 1));
                        if classify_nodes && classify_node(&dense_node_data(&n)) {
                            selected.insert(n.id());
                        }
                    } else if extra_node_ids.contains(&n.id()) {
                        out.push((n.id(), n.lon() as f32, n.lat() as f32, false));
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
                    }
                }
            }
            Ok((out, selected))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;

    let mut coords: NodeCoords = FxHashMap::default();
    let mut selected: FxHashSet<i64> = FxHashSet::default();
    for (chunk, sel) in per_blob {
        for (id, lon, lat, shared) in chunk {
            coords.insert(id, (lon, lat, shared));
        }
        selected.extend(sel);
    }
    Ok((coords, selected))
}
