//! Fallback: full parallel scans for unsorted / non-seekable-ordered files, or when the boundary
//! check fails. Rare and slower, but behaviorally identical to the sorted path. Runs the relations
//! scan first (to build the member-way keep set), then classifies ways (holding kept ways' node ids
//! in memory), then collects node coords + classifies nodes, then resolves geometry.

use anyhow::Context;
use rustc_hash::{FxHashMap, FxHashSet};
use osmpbf::{Element, ElementReader};
use tracing::info;

use crate::osm::types::{NodeData, OsmWay, RelData, WayData};

use super::resolve::{
    dense_node_data, log_node_summary, node_data, rel_data, resolve_geometry, way_data, NodeCoords,
};
use super::Callbacks;

pub(super) fn stream_osm_fallback<CR, CW, CN, G, M>(
    path: &str,
    cb: Callbacks<CR, CW, CN, G>,
) -> anyhow::Result<()>
where
    CR: Fn(&RelData) -> bool + Sync + Send,
    CW: Fn(&WayData, bool) -> Option<M> + Sync + Send,
    CN: Fn(&NodeData) -> bool + Sync + Send,
    G: Fn(&OsmWay, M) + Sync + Send,
    M: Copy + Send + Sync,
{
    // Scan R — relations: emit rows + collect the member-way keep set.
    let keep_set: FxHashSet<i64> = if cb.has_relations {
        info!("Fallback scan R (parallel): classify relations...");
        ElementReader::from_path(path)
            .context("opening PBF for relation scan")?
            .par_map_reduce(
                |element| {
                    let mut keep = FxHashSet::default();
                    if let Element::Relation(rel) = element {
                        let rd = rel_data(&rel);
                        if (cb.classify_rel)(&rd) {
                            keep.extend(rd.member_ways.iter().copied());
                        }
                    }
                    keep
                },
                FxHashSet::default,
                |mut a, b: FxHashSet<i64>| {
                    a.extend(b);
                    a
                },
            )
            .context("relation scan parallel read")?
    } else {
        FxHashSet::default()
    };

    // Scan 1 — ways: classify (emit tag rows), keep kept ways' (id, refs, payload) + tally use counts.
    info!("Fallback scan 1 (parallel): classify ways...");
    let (use_counts, kept_ways): (FxHashMap<i64, u32>, Vec<(i64, Vec<i64>, M)>) =
        ElementReader::from_path(path)
            .context("opening PBF for way scan")?
            .par_map_reduce(
                |element| {
                    let mut counts: FxHashMap<i64, u32> = FxHashMap::default();
                    let mut ways: Vec<(i64, Vec<i64>, M)> = Vec::new();
                    if let Element::Way(way) = element {
                        let wd = way_data(&way);
                        let in_keep = keep_set.contains(&wd.id);
                        if let Some(m) = (cb.classify_way)(&wd, in_keep) {
                            for &id in &wd.node_refs {
                                *counts.entry(id).or_insert(0) += 1;
                            }
                            ways.push((wd.id, wd.node_refs, m));
                        }
                    }
                    (counts, ways)
                },
                || (FxHashMap::default(), Vec::new()),
                |mut a, mut b| {
                    if a.0.len() < b.0.len() {
                        std::mem::swap(&mut a, &mut b);
                    }
                    for (k, v) in b.0 {
                        *a.0.entry(k).or_insert(0) += v;
                    }
                    a.1.extend(b.1);
                    a
                },
            )
            .context("way scan parallel read")?;
    drop(keep_set);

    // Scan 2 — nodes: coords for needed nodes (+ classify nodes → selected set).
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

    // Geometry — resolve each kept way (held in memory) + build geometry. Selected nodes forced.
    info!("Fallback geometry pass: resolve + build geometry...");
    use rayon::prelude::*;
    kept_ways.par_iter().for_each(|(id, refs, m)| {
        if let Some(w) = resolve_geometry(*id, refs, &coords, &selected) {
            (cb.build_geom)(&w, *m);
        }
    });

    Ok(())
}
