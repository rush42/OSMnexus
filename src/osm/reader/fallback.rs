//! Fallback: full parallel scans for unsorted / non-seekable-ordered files, or when the boundary
//! check fails. Rare and slower, but behaviorally identical to the sorted path. Runs the relations
//! scan first (classify + emit only — independent of the passes below), then classifies ways
//! (holding kept ways' node ids in memory), then collects node coords + classifies nodes, then
//! resolves geometry.

use anyhow::Context;
use rustc_hash::{FxHashMap, FxHashSet};
use osmpbf::{Element, ElementReader};
use tracing::info;

use crate::osm::types::{NodeData, OsmWay, RelData, WayData};

use super::resolve::{
    dense_node_data, log_node_summary, node_data, rel_data, resolve_geometry, way_data, NodeCoords,
};
use super::{assign_node_ids, resolve_extra_way_coords, Callbacks};

pub(super) fn stream_osm_fallback<CR, CW, CN, G, BN, BR, M>(
    path: &str,
    cb: Callbacks<CR, CW, CN, G, BN, BR>,
) -> anyhow::Result<()>
where
    CR: Fn(&RelData) -> bool + Sync + Send,
    CW: Fn(&WayData) -> Option<M> + Sync + Send,
    CN: Fn(&NodeData) -> bool + Sync + Send,
    G: Fn(&OsmWay, M, &FxHashMap<i64, i64>) + Sync + Send,
    BN: FnOnce(Vec<(i64, i64, f32, f32)>),
    BR: FnOnce(FxHashMap<i64, Vec<(f64, f64)>>),
    M: Copy + Send + Sync,
{
    // Scan R — relations: classify + emit rows. Independent of the passes below. Sequential (the
    // relation region is small relative to ways/nodes; not worth a `par_map_reduce` for a scan with
    // no keep-set to accumulate).
    if cb.has_relations {
        info!("Fallback scan R: classify relations...");
        ElementReader::from_path(path)
            .context("opening PBF for relation scan")?
            .for_each(|element| {
                if let Element::Relation(rel) = element {
                    (cb.classify_rel)(&rel_data(&rel));
                }
            })
            .context("relation scan read")?;
    }
    // `extra_way_ids` is fully populated now — see `Callbacks::extra_way_ids`'s own doc.
    let extra_way_ids: FxHashSet<i64> = std::mem::take(&mut *cb.extra_way_ids.lock().unwrap());

    // Scan 1 — ways: classify (emit tag rows), keep kept ways' (id, refs, payload) + tally use counts
    // + endpoints. A way is kept purely by its own tag classification. Independently, any
    // `extra_way_ids` member way's node refs are recorded into `extra_way_refs` too (see
    // `sorted::classify_and_index`'s own doc on this same side channel).
    info!("Fallback scan 1 (parallel): classify ways...");
    let (use_counts, endpoints, kept_ways, extra_way_refs): (
        FxHashMap<i64, u32>,
        FxHashSet<i64>,
        Vec<(i64, Vec<i64>, M)>,
        FxHashMap<i64, Vec<i64>>,
    ) = ElementReader::from_path(path)
        .context("opening PBF for way scan")?
        .par_map_reduce(
            |element| {
                let mut counts: FxHashMap<i64, u32> = FxHashMap::default();
                let mut endpoints: FxHashSet<i64> = FxHashSet::default();
                let mut ways: Vec<(i64, Vec<i64>, M)> = Vec::new();
                let mut extra_way_refs: FxHashMap<i64, Vec<i64>> = FxHashMap::default();
                if let Element::Way(way) = element {
                    let wd = way_data(&way);
                    if !extra_way_ids.is_empty() && extra_way_ids.contains(&wd.id) {
                        extra_way_refs.insert(wd.id, wd.node_refs.clone());
                    }
                    if let Some(m) = (cb.classify_way)(&wd) {
                        for &id in &wd.node_refs {
                            *counts.entry(id).or_insert(0) += 1;
                        }
                        if let (Some(&first), Some(&last)) = (wd.node_refs.first(), wd.node_refs.last()) {
                            endpoints.insert(first);
                            endpoints.insert(last);
                        }
                        ways.push((wd.id, wd.node_refs, m));
                    }
                }
                (counts, endpoints, ways, extra_way_refs)
            },
            || (FxHashMap::default(), FxHashSet::default(), Vec::new(), FxHashMap::default()),
            |mut a, mut b| {
                if a.0.len() < b.0.len() {
                    std::mem::swap(&mut a, &mut b);
                }
                for (k, v) in b.0 {
                    *a.0.entry(k).or_insert(0) += v;
                }
                a.1.extend(b.1);
                a.2.extend(b.2);
                a.3.extend(b.3);
                a
            },
        )
        .context("way scan parallel read")?;

    // Scan 2 — nodes: coords for needed nodes (+ classify nodes → selected set). Widened to also
    // cover `extra_way_refs`' node ids, same as the sorted path's `collect_coords`.
    let extra_node_ids: FxHashSet<i64> = extra_way_refs.values().flatten().copied().collect();
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
    let coords: NodeCoords = coords_vec
        .into_iter()
        .map(|(id, lon, lat)| {
            let shared = use_counts.get(&id).copied().unwrap_or(0) > 1;
            (id, (lon, lat, shared))
        })
        .collect();

    log_node_summary(&use_counts);
    drop(use_counts);

    (cb.build_extra_geom)(resolve_extra_way_coords(extra_way_refs, &coords));

    // Assign internal node ids to every graph vertex + emit the `nodes` table rows (see
    // `assign_node_ids` — sequential, so the resulting map is read-only across the parallel geometry
    // pass below).
    let (node_ids, node_rows) = assign_node_ids(&coords, &endpoints, &selected);
    drop(endpoints);
    (cb.build_nodes)(node_rows);

    // Geometry — resolve each kept way (held in memory) + build geometry. Selected nodes forced.
    info!("Fallback geometry pass: resolve + build geometry...");
    use rayon::prelude::*;
    kept_ways.par_iter().for_each(|(id, refs, m)| {
        if let Some(w) = resolve_geometry(*id, refs, &coords, &selected) {
            (cb.build_geom)(&w, *m, &node_ids);
        }
    });

    Ok(())
}
