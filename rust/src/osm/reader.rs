use std::collections::HashMap;

use anyhow::Context;
use osmpbf::{Element, ElementReader};
use rayon::prelude::*;
use tracing::info;

use super::types::{OsmWay, RawTags, WayMeta};

/// Intermediate way data before geometry resolution.
struct WayData {
    id: i64,
    tags: RawTags,
    node_refs: Vec<i64>,
    meta: WayMeta,
}

/// Read OSM ways that have a `highway` tag from a PBF file.
///
/// Two parallel passes:
///   Pass 1: collect highway ways (tags + node refs) and the node IDs they reference
///   Pass 2: collect coordinates for those nodes
///   Resolve: build geometries from collected data (in parallel with rayon)
pub fn read_highway_ways(path: &str) -> anyhow::Result<Vec<OsmWay>> {
    info!("Pass 1 (parallel): collecting highway ways + referenced node IDs...");
    let (ways_data, node_ids) = collect_ways_and_node_ids(path)?;
    info!(
        "Pass 1 done: {} highway ways, {} referenced nodes",
        ways_data.len(),
        node_ids.len()
    );

    info!("Pass 2 (parallel): collecting node coordinates...");
    let node_coords = collect_node_coords(path, &node_ids)?;
    info!("Pass 2 done: {} node coordinates", node_coords.len());

    info!("Resolving geometries (parallel)...");
    let ways: Vec<OsmWay> = ways_data
        .into_par_iter()
        .filter_map(|wd| resolve_way(wd, &node_coords))
        .collect();
    info!("Resolved: {} ways with valid geometry", ways.len());

    Ok(ways)
}

/// Pass 1 (parallel): collect all highway ways and their referenced node IDs.
fn collect_ways_and_node_ids(
    path: &str,
) -> anyhow::Result<(Vec<WayData>, HashMap<i64, ()>)> {
    let reader = ElementReader::from_path(path).context("opening PBF for pass 1")?;

    // par_map_reduce processes blobs in parallel.
    // Each blob returns (Vec<WayData>, Vec<i64> node_ids).
    let (ways_data, node_ids_vec) = reader
        .par_map_reduce(
            |element| {
                let mut ways: Vec<WayData> = Vec::new();
                let mut node_ids: Vec<i64> = Vec::new();

                if let Element::Way(way) = element {
                    let tags: RawTags =
                        way.tags().map(|(k, v)| (k.to_owned(), v.to_owned())).collect();

                    if !tags.contains_key("highway") {
                        return (ways, node_ids);
                    }

                    let refs: Vec<i64> = way.refs().collect();
                    node_ids.extend_from_slice(&refs);

                    let meta = if way.info().version().is_some() {
                        let info = way.info();
                        WayMeta {
                            timestamp: info.milli_timestamp().map(|ms| ms / 1000),
                            user: info.user().and_then(|r| r.ok()).map(|s| s.to_owned()),
                            changeset: info.changeset(),
                        }
                    } else {
                        WayMeta { timestamp: None, user: None, changeset: None }
                    };

                    ways.push(WayData { id: way.id(), tags, node_refs: refs, meta });
                }
                (ways, node_ids)
            },
            || (Vec::new(), Vec::new()),
            |(mut wa, mut na), (wb, nb)| {
                wa.extend(wb);
                na.extend(nb);
                (wa, na)
            },
        )
        .context("pass 1 parallel read")?;

    // Deduplicate node IDs into a presence map.
    let node_ids: HashMap<i64, ()> = node_ids_vec.into_iter().map(|id| (id, ())).collect();
    Ok((ways_data, node_ids))
}

/// Pass 2 (parallel): collect coordinates for referenced nodes.
/// Stored as f32 to halve memory (~1 m precision at Berlin's latitude).
fn collect_node_coords(
    path: &str,
    node_ids: &HashMap<i64, ()>,
) -> anyhow::Result<HashMap<i64, (f32, f32)>> {
    let reader = ElementReader::from_path(path).context("opening PBF for pass 2")?;

    let coords = reader
        .par_map_reduce(
            |element| {
                let mut coords: Vec<(i64, f32, f32)> = Vec::new();
                match element {
                    Element::DenseNode(n) if node_ids.contains_key(&n.id()) => {
                        coords.push((n.id(), n.lon() as f32, n.lat() as f32));
                    }
                    Element::Node(n) if node_ids.contains_key(&n.id()) => {
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

    Ok(coords.into_iter().map(|(id, lon, lat)| (id, (lon, lat))).collect())
}

/// Resolve a WayData into an OsmWay by looking up node coordinates.
fn resolve_way(wd: WayData, node_coords: &HashMap<i64, (f32, f32)>) -> Option<OsmWay> {
    let coords: Vec<(f64, f64)> = wd
        .node_refs
        .iter()
        .filter_map(|id| node_coords.get(id))
        .map(|&(lon, lat)| (lon as f64, lat as f64))
        .collect();

    if coords.len() < 2 {
        return None;
    }

    Some(OsmWay { id: wd.id, coords, tags: wd.tags, meta: wd.meta })
}
