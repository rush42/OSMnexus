//! The sorted fast path: the relations pass (classify + emit, collect member-way requests), Pass A
//! (classify ways + index, plus a side channel for any `extra_way_ids`), and Pass B (collect node
//! coords + classify nodes, widened for `extra_way_ids`' node ids) — each decoding its blob region
//! once. No geometry pass here — resolving/building geometry is `geom::materialize`'s job, run
//! separately over the `SelectionContext` these passes produce.

use rustc_hash::{FxHashMap, FxHashSet};
use osmpbf::ByteOffset;
use rayon::prelude::*;

use crate::geom::rows::GeomRow;
use crate::osm::types::{MemberRole, NodeData, RelData, WayData};
use crate::output::rows::{MemberRow, TopicRow};

use super::blob_index::decode_block;
use super::resolve::{dense_node_data, node_data, rel_data, way_data, NodeCoords, NodeCoordsBuilder, NodeRefCounts};
use super::way_refs::{EncodedRefs, WayRefsStore};

/// Merge one element's per-topic tag rows into a running per-blob batch, appending rather than
/// replacing so multiple elements' rows for the same topic accumulate in encounter order. Used to
/// turn many small `route_tag` calls (one per element — a single way/node/relation typically
/// produces 1-3 rows) into one call per blob: `Shard::send` buffers small sends behind a per-shard
/// `Mutex` ([`output::sinks`]'s own doc), so calling it once per element from up to `--threads`
/// concurrent rayon workers (the `!ordered` routing path) contends on that mutex on every call —
/// measured as a net *regression* on `germany-latest.osm.pbf` when routing moved into the parallel
/// section at per-element granularity (6:30 batched-nowhere vs 6:58 per-element-unordered, worse
/// than staying in the ordered sequential fold). Batching per blob cuts the call count by roughly
/// the average element-per-blob ratio, collapsing most of that contention regardless of which path
/// (ordered fold or unordered parallel section) is doing the routing.
fn merge_topic_rows(batch: &mut Vec<Vec<TopicRow>>, rows: Vec<Vec<TopicRow>>) {
    if batch.len() < rows.len() {
        batch.resize_with(rows.len(), Vec::new);
    }
    for (i, mut r) in rows.into_iter().enumerate() {
        batch[i].append(&mut r);
    }
}

/// Relations pass — decode the relation region once, in bounded parallel chunks (`FOLD_CHUNK_BLOBS`,
/// same pattern as Pass A/B below), folding each chunk sequentially in blob order. `classify_rel` is
/// pure (no routing); each kept relation's tag rows and member-way links are routed from the
/// sequential fold via `route_tag`/`route_member`, so relation tag-row order is a deterministic
/// function of blob order — previously `classify_rel` routed as a side effect from inside the
/// parallel section, racing across relations decoded concurrently.
pub(super) fn classify_relations<CR, RT, RM>(
    mmap: &osmpbf::Mmap,
    rel_offsets: &[ByteOffset],
    classify_rel: &CR,
    route_tag: &RT,
    route_member: &RM,
    ordered: bool,
) -> anyhow::Result<(FxHashMap<i64, (Vec<(i64, MemberRole)>, u32)>, Vec<i64>)>
where
    CR: for<'a> Fn(&RelData<'a>) -> (Option<u32>, Vec<Vec<TopicRow>>, Vec<MemberRow>) + Sync,
    RT: Fn(Vec<Vec<TopicRow>>) + Sync,
    RM: Fn(Vec<MemberRow>) + Sync,
{
    type Routing = Option<(Vec<Vec<TopicRow>>, Vec<MemberRow>)>;
    type KeptRel = (i64, Vec<(i64, MemberRole)>, u32, Routing);
    let mut result: FxHashMap<i64, (Vec<(i64, MemberRole)>, u32)> = FxHashMap::default();
    // Every kept relation's id, in the same blob order its tag row was just routed in — see
    // `sorted::classify_and_index`'s `kept_way_order` for the way-side equivalent of this. Left
    // empty when `!ordered` (nobody reads it — routing already happened from the parallel section).
    let mut kept_relation_order: Vec<i64> = Vec::new();
    for blob_chunk in rel_offsets.chunks(FOLD_CHUNK_BLOBS) {
        let per_blob: Vec<Vec<KeptRel>> = blob_chunk
            .par_iter()
            .map(|&off| -> anyhow::Result<Vec<KeptRel>> {
                let block = decode_block(mmap, off)?;
                let mut out = Vec::new();
                for group in block.groups() {
                    for rel in group.relations() {
                        let rd = rel_data(&rel);
                        let (mask, topic_rows, links) = classify_rel(&rd);
                        if let Some(mask) = mask {
                            if ordered {
                                out.push((rd.id, rd.member_ways, mask, Some((topic_rows, links))));
                            } else {
                                // No ordering to preserve (e.g. `pg` output, joined on `osm_id`
                                // downstream) — route straight from this parallel closure instead
                                // of paying the sequential fold's serialization below.
                                route_tag(topic_rows);
                                if !links.is_empty() {
                                    route_member(links);
                                }
                                out.push((rd.id, rd.member_ways, mask, None));
                            }
                        }
                    }
                }
                Ok(out)
            })
            .collect::<anyhow::Result<Vec<_>>>()?;

        for rels in per_blob {
            for (id, member_ways, mask, routing) in rels {
                if let Some((topic_rows, links)) = routing {
                    route_tag(topic_rows);
                    if !links.is_empty() {
                        route_member(links);
                    }
                    kept_relation_order.push(id);
                }
                result.insert(id, (member_ways, mask));
            }
        }
    }
    Ok((result, kept_relation_order))
}

/// How many blobs a pass decodes in parallel before folding their output sequentially into the
/// pass's accumulators. This bounds the *transient* cost of the decoded-but-not-yet-folded `Vec`s:
/// collecting every blob first (which is what Pass B used to do) meant the whole file's data was
/// resident twice at the moment the fold began — once as per-record `Vec` tuples, once in the
/// growing accumulator. Shared by all three passes (`classify_relations`, `classify_and_index`,
/// `collect_coords`).
///
/// The fold stays sequential and *in blob order* on purpose, for two independent reasons now.
/// `FxHashMap`/`FxHashSet` are unseeded, so their iteration order is a deterministic function of
/// insertion order, and `assign_node_ids` hands out internal graph-vertex ids in exactly that order
/// — reordering insertions would silently renumber every row of the `nodes` and `edges` tables. And
/// every pass now also routes its element's tag rows (`route_tag`/`route_member`/`route_point`)
/// from this same blob-order fold rather than from the parallel section — see each pass's own doc
/// for why (this used to race across elements decoded concurrently within a chunk, so a way's tag
/// row and its later-materialized geometry row landed in unrelated relative positions in their
/// respective output files).
///
/// Sized much smaller than it once was (was `256`, tuned only for the cheap ref/id payload the
/// accumulators held) precisely because the transient now also holds a whole chunk's worth of
/// pre-serialized `TopicRow`s (JSON `String`s) awaiting routing — at `256` this measured +80% peak
/// RSS on `brandenburg-latest.osm.pbf` (550MB → 990MB) for no measurable time benefit; at `16` RSS
/// matches the pre-routing-reorder baseline with no measurable slowdown. Re-measure before raising
/// this if decode ever shows up as CPU-bound on the folding step (it hasn't).
const FOLD_CHUNK_BLOBS: usize = 64;

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
    /// This way's per-topic tag rows, routed from the sequential fold below (not from the parallel
    /// decode section) so tag-row order is a deterministic function of blob order — see
    /// `classify_and_index`'s own doc.
    topic_rows: Vec<Vec<TopicRow>>,
}

/// Pass A — decode the way-region blobs once, in bounded parallel chunks (`FOLD_CHUNK_BLOBS`, same
/// pattern as Pass B's `collect_coords`), folding each chunk's ways sequentially into three
/// accumulators, plus routing each way's tag rows via `route_tag` — from the fold, not from the
/// parallel section, so tag-row order is a deterministic function of blob order (previously
/// `classify` routed as a side effect from inside the parallel closure, racing across ways decoded
/// concurrently within a chunk). `classify` itself is now pure: it returns the keep mask and the
/// tag rows instead of routing them. A way is *kept* (mask != 0) when `classify` returns `Some` —
/// purely its own tag classification, independent of relation membership. Every kept way, plus
/// every way in `extra_way_ids` (a
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
pub(super) fn classify_and_index<C, RT>(
    mmap: &osmpbf::Mmap,
    way_offsets: &[ByteOffset],
    classify: &C,
    route_tag: &RT,
    extra_way_ids: &FxHashSet<i64>,
    needs_graph: bool,
    ordered: bool,
) -> anyhow::Result<(NodeRefCounts, WayRefsStore, FxHashSet<i64>, Vec<i64>)>
where
    C: for<'a> Fn(&WayData<'a>) -> (Option<u32>, Vec<Vec<TopicRow>>) + Sync,
    RT: Fn(Vec<Vec<TopicRow>>) + Sync,
{
    let mut counts: FxHashMap<i64, u32> = FxHashMap::default();
    // Appended to unsorted (with duplicates — a node referenced by k ways lands k times), then
    // sorted and deduplicated once at the end: pushing is cheaper than hashing per ref, and the
    // final array is exactly as large as the distinct-id count.
    let mut present: Vec<i64> = Vec::new();
    // Pushed, not inserted into a map — `WayRefsStore::build` immediately re-sorts every record
    // into MPHF-slot order anyway (`store::MphfArena::build`), so accumulating a `FxHashMap` here
    // only to drain it into a `Vec` right after paid for a hash + probe on every one of tens of
    // millions of kept ways for an ordering guarantee nothing downstream reads. Plain `push` is O(1)
    // amortized with no hashing at all.
    let mut way_refs: Vec<(i64, EncodedRefs, u32)> = Vec::new();
    let mut extra_node_ids: FxHashSet<i64> = FxHashSet::default();
    // Every tag-kept (mask != 0) way's id, in the same blob order its tag row was just routed in —
    // handed back so `geom::materialize::run` can route that way's geometry row in matching order
    // too (`WayRefsStore::par_route_ordered`), instead of the arena's own MPHF-slot order. One `i64`
    // per kept way — for `germany-latest.osm.pbf`'s tens of millions of kept ways this is tens to
    // ~100s of MB, the same order of magnitude `way_refs` itself already holds.
    let mut kept_way_order: Vec<i64> = Vec::new();

    for blob_chunk in way_offsets.chunks(FOLD_CHUNK_BLOBS) {
        let mut per_blob: Vec<Vec<ClassifiedWay>> = blob_chunk
            .par_iter()
            .map(|&off| -> anyhow::Result<Vec<ClassifiedWay>> {
                let block = decode_block(mmap, off)?;
                let mut out = Vec::new();
                // Only populated when `!ordered` — one `route_tag` call per blob instead of one per
                // way, called from this parallel closure (see `merge_topic_rows`'s own doc).
                let mut tag_batch: Vec<Vec<TopicRow>> = Vec::new();
                for group in block.groups() {
                    for way in group.ways() {
                        let wd = way_data(&way);
                        let (kept_mask, topic_rows) = classify(&wd);
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
                        let topic_rows = if ordered {
                            topic_rows
                        } else {
                            merge_topic_rows(&mut tag_batch, topic_rows);
                            Vec::new()
                        };
                        out.push(ClassifiedWay {
                            id: wd.id,
                            refs: EncodedRefs::from_deltas(deltas),
                            mask: kept_mask.unwrap_or(0),
                            node_ids,
                            topic_rows,
                        });
                    }
                }
                if !ordered {
                    // No downstream ordering to preserve (e.g. `pg` output, joined on `osm_id`) —
                    // route straight from this parallel closure instead of carrying the tag rows
                    // into the sequential fold just to route them one blob-order barrier later.
                    route_tag(tag_batch);
                }
                Ok(out)
            })
            .collect::<anyhow::Result<Vec<_>>>()?;

        // Batch the whole chunk's ways' tag rows into one `route_tag` call per chunk instead of one
        // per way (only relevant when `ordered` — the unordered path already batched per blob and
        // left `w.topic_rows` empty above) — same contention reasoning as `merge_topic_rows`'s own
        // doc, applied to this sequential fold (uncontended here, but still one mutex lock/unlock
        // per call otherwise).
        let mut tag_batch: Vec<Vec<TopicRow>> = Vec::new();
        for ways in &mut per_blob {
            for w in ways {
                if ordered {
                    merge_topic_rows(&mut tag_batch, std::mem::take(&mut w.topic_rows));
                }
            }
        }
        if ordered {
            route_tag(tag_batch);
        }

        for ways in per_blob {
            for w in ways {
                match &w.node_ids {
                    Some(ids) => {
                        if ordered {
                            kept_way_order.push(w.id);
                        }
                        for &nid in ids {
                            if needs_graph {
                                *counts.entry(nid).or_insert(0) += 1;
                            } else {
                                present.push(nid);
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
                way_refs.push((w.id, w.refs, w.mask));
            }
        }
    }

    let use_counts = if needs_graph {
        NodeRefCounts::Counted(counts)
    } else {
        // Sorting is what makes the array form work at all: `binary_search` needs the order, and
        // `dedup` only collapses *consecutive* equals, so the duplicates (a node referenced by k
        // ways was pushed k times) only fold away once sorted. Done in parallel because it's the
        // one genuinely new cost this representation adds to Pass A — serially it measured ~8s on
        // germany's ~91M pushed refs, against ~5s saved in Pass B.
        present.par_sort_unstable();
        present.dedup();
        present.shrink_to_fit();
        NodeRefCounts::Present(present)
    };
    Ok((use_counts, WayRefsStore::build_records(way_refs), extra_node_ids, kept_way_order))
}

/// Pass B — collect coordinates (as fixed-point decimicrodegrees, the PBF's own exact integer form
/// — see `NodeCoords`) for the needed nodes from the node-region blobs, in parallel (in chunks of
/// `FOLD_CHUNK_BLOBS`, see its doc). Each node's `shared` flag
/// (used by ≥2 mask-!=0 ways) is read from `use_counts` here and baked into the value.
///
/// When `classify_nodes` is set, every needed node's tags are also decoded and passed to
/// `classify_node`, which is now pure — it returns whether the node is *selected* (forced
/// graph-vertex cut point), its tag rows, and (if kept) its point row, instead of routing them as a
/// side effect. Routing happens from this function's sequential, blob-order fold via `route_tag`/
/// `route_point`, same reasoning as Pass A (`classify_and_index`'s own doc) — previously
/// `classify_node` routed from inside the parallel section, racing across nodes decoded
/// concurrently within a chunk. When `classify_nodes` is unset, node tags are not decoded at all —
/// preserving the way-only performance profile.
///
/// `extra_node_ids` widens which nodes get a coordinate fetched (mask-`0` `way_refs`' node ids),
/// but never affects `shared` (always computed from `use_counts` alone, `false` for an extra-only
/// node) or node-topic classification (only run for `use_counts` members) — pure coordinate
/// lookup, no side effects on the main graph's intersection/cut-point logic.
pub(super) fn collect_coords<CN, RT, RP>(
    mmap: &osmpbf::Mmap,
    node_offsets: &[ByteOffset],
    use_counts: &NodeRefCounts,
    classify_nodes: bool,
    classify_node: &CN,
    route_tag: &RT,
    route_point: &RP,
    extra_node_ids: &FxHashSet<i64>,
    skip_untagged: bool,
    ordered: bool,
) -> anyhow::Result<(NodeCoords, FxHashSet<i64>, u64)>
where
    CN: for<'a> Fn(&NodeData<'a>) -> (bool, Vec<Vec<TopicRow>>, Option<(u32, GeomRow)>) + Sync,
    RT: Fn(Vec<Vec<TopicRow>>) + Sync,
    RP: Fn(u32, GeomRow) + Sync,
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

    type NodeClassified = (Vec<Vec<TopicRow>>, Option<(u32, GeomRow)>);
    for blob_chunk in node_offsets.chunks(FOLD_CHUNK_BLOBS) {
        type BlobResult = (Vec<(i64, i32, i32, bool)>, FxHashSet<i64>, u64, Vec<NodeClassified>);
        let per_blob: Vec<BlobResult> = blob_chunk
            .par_iter()
            .map(|&off| -> anyhow::Result<BlobResult> {
                let block = decode_block(mmap, off)?;
                let mut out = Vec::new();
                let mut selected: FxHashSet<i64> = FxHashSet::default();
                let mut standalone: u64 = 0;
                let mut classified: Vec<NodeClassified> = Vec::new();
                // No downstream ordering to preserve (e.g. `pg` output) — route straight from here
                // instead of carrying tag rows into the sequential fold. `route_point` was never
                // order-dependent to begin with (a node's point row is only ever correlated with
                // its own tag row from the same `classify_node` call, never with another element's).
                // Tag rows batch per blob (`tag_batch`, flushed after the group loop) rather than
                // routing per node — see `merge_topic_rows`'s own doc for why. `route_point` stays
                // per-call: it already batches per shard behind `Shard::send`'s own `SEND_BATCH`
                // buffer, so there's no equivalent per-call contention to fix here.
                let mut tag_batch: Vec<Vec<TopicRow>> = Vec::new();
                let route_now = |tag_batch: &mut Vec<Vec<TopicRow>>, topic_rows, point: Option<(u32, GeomRow)>| {
                    merge_topic_rows(tag_batch, topic_rows);
                    if let Some((mask, row)) = point {
                        route_point(mask, row);
                    }
                };
                for group in block.groups() {
                    for n in group.dense_nodes() {
                        if let Some(shared) = use_counts.lookup(&n.id()) {
                            out.push((n.id(), n.decimicro_lon(), n.decimicro_lat(), shared));
                            if classify_nodes && !(skip_untagged && n.tags().next().is_none()) {
                                let (forced_cut, topic_rows, point) = classify_node(&dense_node_data(&n));
                                if forced_cut {
                                    selected.insert(n.id());
                                }
                                if ordered {
                                    classified.push((topic_rows, point));
                                } else {
                                    route_now(&mut tag_batch, topic_rows, point);
                                }
                            }
                        } else if extra_node_ids.contains(&n.id()) {
                            out.push((n.id(), n.decimicro_lon(), n.decimicro_lat(), false));
                        } else if classify_nodes && !(skip_untagged && n.tags().next().is_none()) {
                            // Not part of any kept way — still classify it (tag rows / point
                            // geometry are driven by `classify_node` itself from `NodeData`, not
                            // `NodeCoords`), just don't hold its coords or count it toward graph
                            // cut points.
                            let (_, topic_rows, point) = classify_node(&dense_node_data(&n));
                            if ordered {
                                classified.push((topic_rows, point));
                            } else {
                                route_now(&mut tag_batch, topic_rows, point);
                            }
                            standalone += 1;
                        }
                    }
                    for n in group.nodes() {
                        if let Some(shared) = use_counts.lookup(&n.id()) {
                            out.push((n.id(), n.decimicro_lon(), n.decimicro_lat(), shared));
                            if classify_nodes && !(skip_untagged && n.tags().next().is_none()) {
                                let (forced_cut, topic_rows, point) = classify_node(&node_data(&n));
                                if forced_cut {
                                    selected.insert(n.id());
                                }
                                if ordered {
                                    classified.push((topic_rows, point));
                                } else {
                                    route_now(&mut tag_batch, topic_rows, point);
                                }
                            }
                        } else if extra_node_ids.contains(&n.id()) {
                            out.push((n.id(), n.decimicro_lon(), n.decimicro_lat(), false));
                        } else if classify_nodes && !(skip_untagged && n.tags().next().is_none()) {
                            let (_, topic_rows, point) = classify_node(&node_data(&n));
                            if ordered {
                                classified.push((topic_rows, point));
                            } else {
                                route_now(&mut tag_batch, topic_rows, point);
                            }
                            standalone += 1;
                        }
                    }
                }
                if !ordered {
                    route_tag(tag_batch);
                }
                Ok((out, selected, standalone, classified))
            })
            .collect::<anyhow::Result<Vec<_>>>()?;

        // Batch the whole chunk's nodes' tag rows into one `route_tag` call (only relevant when
        // `ordered` — the unordered path already flushed its own per-blob batch above and left
        // `classified` empty) — see `merge_topic_rows`'s own doc.
        let mut tag_batch: Vec<Vec<TopicRow>> = Vec::new();
        for (chunk, sel, standalone, classified) in per_blob {
            for (id, lon, lat, shared) in chunk {
                coords.insert(id, lon, lat, shared);
            }
            selected.extend(sel);
            standalone_total += standalone;
            if ordered {
                for (topic_rows, point) in classified {
                    merge_topic_rows(&mut tag_batch, topic_rows);
                    if let Some((mask, row)) = point {
                        route_point(mask, row);
                    }
                }
            }
        }
        if ordered {
            route_tag(tag_batch);
        }
    }
    Ok((coords.finish(), selected, standalone_total))
}
