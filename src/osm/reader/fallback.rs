//! Fallback: three full parallel scans (refs → coords → classify+geometry) for unsorted /
//! non-seekable-ordered files, or when the boundary check fails. Rare; re-decodes the way region in
//! pass 3 rather than holding the node-id index, but is otherwise behaviorally identical.

use anyhow::Context;
use rustc_hash::FxHashMap;
use osmpbf::{Element, ElementReader};
use tracing::info;

use crate::osm::types::{ElementFilter, OsmWay, WayData};

use super::resolve::{log_node_summary, resolve_geometry, way_data, way_passes, NodeCoords};

pub(super) fn stream_ways_fallback<C, G, M>(
    path: &str,
    filters: &[ElementFilter],
    classify: C,
    build_geom: G,
) -> anyhow::Result<()>
where
    C: Fn(&WayData) -> Option<M> + Sync + Send,
    G: Fn(&OsmWay, M) + Sync + Send,
    M: Copy + Send + Sync,
{
    info!("Pass 1 (parallel): collecting referenced node ids...");
    let all_refs: Vec<i64> = ElementReader::from_path(path)
        .context("opening PBF for pass 1")?
        .par_map_reduce(
            |element| {
                let mut refs = Vec::new();
                if let Element::Way(way) = element {
                    if way_passes(filters, &way) {
                        refs.extend(way.refs());
                    }
                }
                refs
            },
            Vec::new,
            |mut a, b| {
                a.extend(b);
                a
            },
        )
        .context("pass 1 parallel read")?;

    let mut use_counts: FxHashMap<i64, u32> = FxHashMap::default();
    for id in all_refs {
        *use_counts.entry(id).or_insert(0) += 1;
    }

    info!("Pass 2 (parallel): collecting node coordinates...");
    let coords_vec = ElementReader::from_path(path)
        .context("opening PBF for pass 2")?
        .par_map_reduce(
            |element| {
                let mut coords: Vec<(i64, f32, f32)> = Vec::new();
                match element {
                    Element::DenseNode(n) if use_counts.contains_key(&n.id()) => {
                        coords.push((n.id(), n.lon() as f32, n.lat() as f32));
                    }
                    Element::Node(n) if use_counts.contains_key(&n.id()) => {
                        coords.push((n.id(), n.lon() as f32, n.lat() as f32));
                    }
                    _ => {}
                }
                coords
            },
            Vec::new,
            |mut a, b| {
                a.extend(b);
                a
            },
        )
        .context("pass 2 parallel read")?;
    let coords: NodeCoords = coords_vec
        .into_iter()
        .map(|(id, lon, lat)| {
            let shared = use_counts.get(&id).copied().unwrap_or(0) > 1;
            (id, (lon, lat, shared))
        })
        .collect();

    log_node_summary(&use_counts);
    drop(use_counts);

    // Pass 3 re-decodes the ways (rare path) so it can classify + resolve + build geometry in one
    // go — coords are already in hand, so no node-id index is needed.
    info!("Pass 3 (parallel): classify + build geometry, streaming...");
    ElementReader::from_path(path)
        .context("opening PBF for pass 3")?
        .par_map_reduce(
            |element| {
                if let Element::Way(way) = element {
                    if way_passes(filters, &way) {
                        let wd = way_data(&way);
                        if let Some(m) = classify(&wd) {
                            if let Some(w) = resolve_geometry(wd.id, &wd.node_refs, &coords) {
                                build_geom(&w, m);
                            }
                        }
                    }
                }
            },
            || (),
            |_, _| (),
        )
        .context("pass 3 parallel read")?;

    Ok(())
}
