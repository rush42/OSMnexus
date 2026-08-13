//! Fallback: full parallel scans for unsorted / non-seekable-ordered files, or when the boundary
//! check fails. Rare and slower, but behaviorally identical to the sorted path — same
//! `SelectionContext` output, no geometry pass here either (see `osm::reader`'s own doc). Runs the
//! relations scan first (classify + emit, collect member-way requests), then classifies ways
//! (holding every kept/relation-member way's node refs in memory), then collects node coords +
//! classifies nodes.

use anyhow::Context;
use rustc_hash::{FxHashMap, FxHashSet};
use osmpbf::{Element, ElementReader};
use tracing::info;

use crate::geom::rows::PointRow;
use crate::osm::types::{MemberRole, NodeData, RelData, WayData};
use crate::output::rows::{MemberRow, TopicRow};

use super::rel_members::RelMembers;
use super::resolve::{dense_node_data, log_node_summary, node_data, rel_data, way_data, NodeCoordsBuilder, NodeRefCounts};
use super::way_refs::{EncodedRefs, WayRefsStore};
use super::{Callbacks, SelectionContext};

/// Rare/slow path — routing happens right next to classification here (no ordering guarantee to
/// preserve, unlike the sorted fast path's blob-order fold — see `Callbacks`' own doc).
pub(super) fn stream_osm_fallback<CR, CW, CN, RT, RM, RP>(
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
    // Scan R — relations: classify + emit rows, collect member-way requests. Independent of the
    // ways/nodes scans below. Sequential (the relation region is small relative to ways/nodes).
    let rel_members: FxHashMap<i64, (Vec<(i64, MemberRole)>, u32)> = if cb.has_relations {
        info!("Fallback scan R: classify relations...");
        let mut out = FxHashMap::default();
        ElementReader::from_path(path)
            .context("opening PBF for relation scan")?
            .for_each(|element| {
                if let Element::Relation(rel) = element {
                    let rd = rel_data(&rel);
                    let (mask, topic_rows, links) = (cb.classify_rel)(&rd);
                    (cb.route_tag)(topic_rows);
                    if !links.is_empty() {
                        (cb.route_member)(links);
                    }
                    if let Some(mask) = mask {
                        out.insert(rd.id, (rd.member_ways, mask));
                    }
                }
            })
            .context("relation scan read")?;
        out
    } else {
        FxHashMap::default()
    };
    // No ordering guarantee to preserve here (see this function's own doc) — just the full
    // kept-relation id set, in whatever order `rel_members`' hashmap iterates.
    let kept_relation_order: Vec<i64> = rel_members.keys().copied().collect();
    // Only relations whose mask wants some geometry pull their member ways' coords in — see
    // `sorted`'s own path for why (mirrored here for the fallback scan).
    let extra_way_ids: FxHashSet<i64> = rel_members
        .values()
        .filter(|(_, mask)| mask & cb.relation_geom_mask != 0)
        .flat_map(|(members, _)| members.iter().map(|&(w, _)| w))
        .collect();

    // Scan 1 — ways: classify (emit tag rows), record every kept or relation-member way's
    // (node_refs, mask) — mask 0 for a relation-only way (see `sorted::classify_and_index`'s doc).
    // `needs_graph` (`cb.needs_graph`, `plan.any_way_graph` at the top-level call site) gates
    // whether `counts` actually tracks per-node reference counts or just membership — see
    // `NodeRefCounts`'s own doc. Unlike the sorted fast path, this keeps one shared `FxHashMap<i64,
    // u32>` accumulator regardless (only shrinking to a plain id set at the very end when
    // `!needs_graph`): the fallback scan is already the rare, slower path (unsorted files / boundary
    // check failures), so the transient isn't worth a second duplicated closure here.
    info!("Fallback scan 1 (parallel): classify ways...");
    let (counts, way_refs): (FxHashMap<i64, u32>, FxHashMap<i64, (EncodedRefs, u32)>) = ElementReader::from_path(path)
        .context("opening PBF for way scan")?
        .par_map_reduce(
            |element| {
                let mut counts: FxHashMap<i64, u32> = FxHashMap::default();
                let mut way_refs: FxHashMap<i64, (EncodedRefs, u32)> = FxHashMap::default();
                if let Element::Way(way) = element {
                    let wd = way_data(&way);
                    let (kept_mask, topic_rows) = (cb.classify_way)(&wd);
                    (cb.route_tag)(topic_rows);
                    let is_extra = !extra_way_ids.is_empty() && extra_way_ids.contains(&wd.id);
                    if kept_mask.is_some() || is_extra {
                        // Deltas straight off the wire (see `way_data`'s own doc) — accumulate to
                        // absolute ids only where needed (counts), pass deltas straight through to
                        // `EncodedRefs` otherwise.
                        let deltas = way.raw_refs();
                        if kept_mask.is_some() {
                            let mut cur = 0i64;
                            for &delta in deltas {
                                cur += delta;
                                if cb.needs_graph {
                                    *counts.entry(cur).or_insert(0) += 1;
                                } else {
                                    counts.entry(cur).or_insert(0);
                                }
                            }
                        }
                        way_refs.insert(wd.id, (EncodedRefs::from_deltas(deltas), kept_mask.unwrap_or(0)));
                    }
                }
                (counts, way_refs)
            },
            || (FxHashMap::default(), FxHashMap::default()),
            |mut a, b| {
                for (k, v) in b.0 {
                    *a.0.entry(k).or_insert(0) += v;
                }
                a.1.extend(b.1);
                a
            },
        )
        .context("way scan parallel read")?;
    // No ordering guarantee to preserve here (see this function's own doc) — just the full
    // tag-kept id set, in whatever order `way_refs`' hashmap iterates.
    let kept_way_order: Vec<i64> =
        way_refs.iter().filter(|(_, (_, mask))| *mask != 0).map(|(&id, _)| id).collect();
    let use_counts = if cb.needs_graph {
        NodeRefCounts::Counted(counts)
    } else {
        let mut ids: Vec<i64> = counts.into_keys().collect();
        ids.sort_unstable();
        NodeRefCounts::Present(ids)
    };

    // Scan 2 — nodes: coords for every referenced node (+ classify nodes → selected set).
    let extra_node_ids: FxHashSet<i64> = way_refs
        .iter()
        .filter(|(_, (_, mask))| *mask == 0)
        .flat_map(|(_, (refs, _))| refs.iter())
        .collect();
    info!("Fallback scan 2 (parallel): collect node coords{}...", if cb.has_nodes { " + classify nodes" } else { "" });
    let (coords_vec, selected, standalone_classified): (Vec<(i64, i32, i32)>, FxHashSet<i64>, u64) =
        ElementReader::from_path(path)
        .context("opening PBF for node scan")?
        .par_map_reduce(
            |element| {
                let mut coords: Vec<(i64, i32, i32)> = Vec::new();
                let mut selected: FxHashSet<i64> = FxHashSet::default();
                let mut standalone: u64 = 0;
                match element {
                    Element::DenseNode(n) if use_counts.contains_key(&n.id()) => {
                        coords.push((n.id(), n.decimicro_lon(), n.decimicro_lat()));
                        if cb.has_nodes {
                            let (forced_cut, topic_rows, point) = (cb.classify_node)(&dense_node_data(&n));
                            (cb.route_tag)(topic_rows);
                            if let Some((mask, row)) = point {
                                (cb.route_point)(mask, row);
                            }
                            if forced_cut {
                                selected.insert(n.id());
                            }
                        }
                    }
                    Element::Node(n) if use_counts.contains_key(&n.id()) => {
                        coords.push((n.id(), n.decimicro_lon(), n.decimicro_lat()));
                        if cb.has_nodes {
                            let (forced_cut, topic_rows, point) = (cb.classify_node)(&node_data(&n));
                            (cb.route_tag)(topic_rows);
                            if let Some((mask, row)) = point {
                                (cb.route_point)(mask, row);
                            }
                            if forced_cut {
                                selected.insert(n.id());
                            }
                        }
                    }
                    Element::DenseNode(n) if extra_node_ids.contains(&n.id()) => {
                        coords.push((n.id(), n.decimicro_lon(), n.decimicro_lat()));
                    }
                    Element::Node(n) if extra_node_ids.contains(&n.id()) => {
                        coords.push((n.id(), n.decimicro_lon(), n.decimicro_lat()));
                    }
                    // Not part of any kept way — still classify it (tag rows / point geometry are
                    // driven by `classify_node` itself from `NodeData`, not `NodeCoords`), just
                    // don't hold its coords or count it toward graph cut points.
                    // Untagged nodes skipped outright when every topic provably yields nothing for
                    // them (see `Callbacks::skip_untagged_nodes`) — same guard the sorted path applies.
                    Element::DenseNode(n)
                        if cb.has_nodes && !(cb.skip_untagged_nodes && n.tags().next().is_none()) =>
                    {
                        let (_, topic_rows, point) = (cb.classify_node)(&dense_node_data(&n));
                        (cb.route_tag)(topic_rows);
                        if let Some((mask, row)) = point {
                            (cb.route_point)(mask, row);
                        }
                        standalone += 1;
                    }
                    Element::Node(n)
                        if cb.has_nodes && !(cb.skip_untagged_nodes && n.tags().next().is_none()) =>
                    {
                        let (_, topic_rows, point) = (cb.classify_node)(&node_data(&n));
                        (cb.route_tag)(topic_rows);
                        if let Some((mask, row)) = point {
                            (cb.route_point)(mask, row);
                        }
                        standalone += 1;
                    }
                    _ => {}
                }
                (coords, selected, standalone)
            },
            || (Vec::new(), FxHashSet::default(), 0u64),
            |mut a, b| {
                a.0.extend(b.0);
                a.1.extend(b.1);
                a.2 += b.2;
                a
            },
        )
        .context("node scan parallel read")?;
    let mut coords_builder = NodeCoordsBuilder::with_capacity(coords_vec.len());
    for (id, lon, lat) in coords_vec {
        let shared = use_counts.lookup(&id).unwrap_or(false);
        coords_builder.insert(id, lon, lat, shared);
    }
    let node_coords = coords_builder.finish();

    log_node_summary(&use_counts, standalone_classified);

    // See the sorted path's own `drop(use_counts)` — nothing past here reads it.
    drop(use_counts);

    let way_refs = WayRefsStore::build(way_refs);
    Ok(SelectionContext {
        node_coords,
        way_refs,
        rel_members: RelMembers::build(rel_members),
        selected,
        kept_way_order,
        kept_relation_order,
    })
}
