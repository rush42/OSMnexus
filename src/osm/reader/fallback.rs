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

use crate::osm::types::{MemberRole, NodeData, RelData, WayData};

use super::resolve::{dense_node_data, log_node_summary, node_data, rel_data, way_data, NodeCoords};
use super::{Callbacks, SelectionContext};

pub(super) fn stream_osm_fallback<CR, CW, CN>(
    path: &str,
    cb: Callbacks<CR, CW, CN>,
) -> anyhow::Result<SelectionContext>
where
    CR: for<'a> Fn(&RelData<'a>) -> Option<u32> + Sync + Send,
    CW: for<'a> Fn(&WayData<'a>) -> Option<u32> + Sync + Send,
    CN: for<'a> Fn(&NodeData<'a>) -> bool + Sync + Send,
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
                    if let Some(mask) = (cb.classify_rel)(&rd) {
                        out.insert(rd.id, (rd.member_ways, mask));
                    }
                }
            })
            .context("relation scan read")?;
        out
    } else {
        FxHashMap::default()
    };
    let extra_way_ids: FxHashSet<i64> =
        rel_members.values().flat_map(|(members, _)| members.iter().map(|&(w, _)| w)).collect();

    // Scan 1 — ways: classify (emit tag rows), record every kept or relation-member way's
    // (node_refs, mask) — mask 0 for a relation-only way (see `sorted::classify_and_index`'s doc).
    info!("Fallback scan 1 (parallel): classify ways...");
    let (use_counts, endpoints, way_refs): (
        FxHashMap<i64, u32>,
        FxHashSet<i64>,
        FxHashMap<i64, (Vec<i64>, u32)>,
    ) = ElementReader::from_path(path)
        .context("opening PBF for way scan")?
        .par_map_reduce(
            |element| {
                let mut counts: FxHashMap<i64, u32> = FxHashMap::default();
                let mut endpoints: FxHashSet<i64> = FxHashSet::default();
                let mut way_refs: FxHashMap<i64, (Vec<i64>, u32)> = FxHashMap::default();
                if let Element::Way(way) = element {
                    let wd = way_data(&way);
                    let kept_mask = (cb.classify_way)(&wd);
                    let is_extra = !extra_way_ids.is_empty() && extra_way_ids.contains(&wd.id);
                    if kept_mask.is_some() || is_extra {
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
                (counts, endpoints, way_refs)
            },
            || (FxHashMap::default(), FxHashSet::default(), FxHashMap::default()),
            |mut a, b| {
                for (k, v) in b.0 {
                    *a.0.entry(k).or_insert(0) += v;
                }
                a.1.extend(b.1);
                a.2.extend(b.2);
                a
            },
        )
        .context("way scan parallel read")?;
    let _ = endpoints; // re-derived from way_refs by geom::materialize (mask != 0 subset)

    // Scan 2 — nodes: coords for every referenced node (+ classify nodes → selected set).
    let extra_node_ids: FxHashSet<i64> = way_refs
        .iter()
        .filter(|(_, (_, mask))| *mask == 0)
        .flat_map(|(_, (refs, _))| refs.iter().copied())
        .collect();
    info!("Fallback scan 2 (parallel): collect node coords{}...", if cb.has_nodes { " + classify nodes" } else { "" });
    let (coords_vec, selected): (Vec<(i64, f32, f32)>, FxHashSet<i64>) = ElementReader::from_path(path)
        .context("opening PBF for node scan")?
        .par_map_reduce(
            |element| {
                let mut coords: Vec<(i64, f32, f32)> = Vec::new();
                let mut selected: FxHashSet<i64> = FxHashSet::default();
                match element {
                    Element::DenseNode(n) if use_counts.contains_key(&n.id()) => {
                        coords.push((n.id(), n.lon() as f32, n.lat() as f32));
                        if cb.has_nodes && (cb.classify_node)(&dense_node_data(&n)) {
                            selected.insert(n.id());
                        }
                    }
                    Element::Node(n) if use_counts.contains_key(&n.id()) => {
                        coords.push((n.id(), n.lon() as f32, n.lat() as f32));
                        if cb.has_nodes && (cb.classify_node)(&node_data(&n)) {
                            selected.insert(n.id());
                        }
                    }
                    Element::DenseNode(n) if extra_node_ids.contains(&n.id()) => {
                        coords.push((n.id(), n.lon() as f32, n.lat() as f32));
                    }
                    Element::Node(n) if extra_node_ids.contains(&n.id()) => {
                        coords.push((n.id(), n.lon() as f32, n.lat() as f32));
                    }
                    _ => {}
                }
                (coords, selected)
            },
            || (Vec::new(), FxHashSet::default()),
            |mut a, b| {
                a.0.extend(b.0);
                a.1.extend(b.1);
                a
            },
        )
        .context("node scan parallel read")?;
    let node_coords: NodeCoords = coords_vec
        .into_iter()
        .map(|(id, lon, lat)| {
            let shared = use_counts.get(&id).copied().unwrap_or(0) > 1;
            (id, (lon, lat, shared))
        })
        .collect();

    log_node_summary(&use_counts);

    Ok(SelectionContext { node_coords, way_refs, rel_members, use_counts, selected })
}
