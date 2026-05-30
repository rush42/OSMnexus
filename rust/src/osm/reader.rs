use std::collections::{HashMap, HashSet};

use anyhow::Context;
use osmpbf::{Element, ElementReader};
use tracing::info;

use super::types::{OsmWay, RawTags, WayMeta};

/// Read OSM ways that have a `highway` tag from a PBF file.
///
/// Uses three logical passes over the file:
///   Pass 1: collect node IDs referenced by highway ways
///   Pass 2: collect coordinates for those nodes
///   Pass 3: assemble OsmWay objects with resolved coordinates
///
/// PBF files have node blobs before way blobs (OSM spec), so passes 1+3
/// can share a single sequential file scan.
pub fn read_highway_ways(path: &str) -> anyhow::Result<Vec<OsmWay>> {
    info!("Pass 1: collecting node IDs referenced by highway ways...");
    let referenced_nodes = collect_referenced_nodes(path)?;
    info!(
        "Pass 1 done: {} nodes referenced by highway ways",
        referenced_nodes.len()
    );

    info!("Pass 2: collecting node coordinates...");
    let node_coords = collect_node_coords(path, &referenced_nodes)?;
    info!("Pass 2 done: {} node coordinates collected", node_coords.len());

    info!("Pass 3: assembling ways...");
    let ways = assemble_ways(path, &node_coords)?;
    info!("Pass 3 done: {} ways assembled", ways.len());

    Ok(ways)
}

/// Pass 1: iterate all ways and collect node IDs referenced by highway ways.
fn collect_referenced_nodes(path: &str) -> anyhow::Result<HashSet<i64>> {
    let reader = ElementReader::from_path(path).context("opening PBF for pass 1")?;
    let mut referenced: HashSet<i64> = HashSet::new();

    reader.for_each(|element| {
        if let Element::Way(way) = element {
            if way.tags().any(|(k, _)| k == "highway") {
                for node_id in way.refs() {
                    referenced.insert(node_id);
                }
            }
        }
    })?;

    Ok(referenced)
}

/// Pass 2: iterate all nodes and collect coordinates for referenced IDs.
/// Coordinates are stored as f32 to reduce memory: <1 m error at EPSG:3857 scale.
fn collect_node_coords(
    path: &str,
    referenced: &HashSet<i64>,
) -> anyhow::Result<HashMap<i64, (f32, f32)>> {
    let reader = ElementReader::from_path(path).context("opening PBF for pass 2")?;
    let mut coords: HashMap<i64, (f32, f32)> = HashMap::with_capacity(referenced.len());

    reader.for_each(|element| match element {
        Element::DenseNode(n) => {
            if referenced.contains(&n.id()) {
                coords.insert(n.id(), (n.lon() as f32, n.lat() as f32));
            }
        }
        Element::Node(n) => {
            if referenced.contains(&n.id()) {
                coords.insert(n.id(), (n.lon() as f32, n.lat() as f32));
            }
        }
        _ => {}
    })?;

    Ok(coords)
}

/// Pass 3: iterate ways again and build OsmWay with resolved coordinates.
fn assemble_ways(
    path: &str,
    node_coords: &HashMap<i64, (f32, f32)>,
) -> anyhow::Result<Vec<OsmWay>> {
    let reader = ElementReader::from_path(path).context("opening PBF for pass 3")?;
    let mut ways: Vec<OsmWay> = Vec::new();

    reader.for_each(|element| {
        if let Element::Way(way) = element {
            let tags: RawTags = way.tags().map(|(k, v)| (k.to_owned(), v.to_owned())).collect();

            if !tags.contains_key("highway") {
                return;
            }

            let coords: Vec<(f64, f64)> = way
                .refs()
                .filter_map(|node_id| {
                    node_coords
                        .get(&node_id)
                        .map(|&(lon, lat)| (lon as f64, lat as f64))
                })
                .collect();

            // Skip ways where we couldn't resolve enough coordinates for a valid geometry.
            if coords.len() < 2 {
                return;
            }

            let meta = if way.info().version().is_some() {
                let info = way.info();
                WayMeta {
                    timestamp: info.milli_timestamp().map(|ms| ms / 1000),
                    user: info.user().and_then(|r| r.ok()).map(|s| s.to_owned()),
                    changeset: info.changeset(),
                }
            } else {
                WayMeta {
                    timestamp: None,
                    user: None,
                    changeset: None,
                }
            };

            ways.push(OsmWay {
                id: way.id(),
                coords,
                tags,
                meta,
            });
        }
    })?;

    Ok(ways)
}
