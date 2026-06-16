use std::collections::HashMap;

use anyhow::{anyhow, Context};
use osmpbf::{
    BlobReader, BlobType, ByteOffset, Element, ElementReader, PrimitiveBlock, Way,
};
use rayon::prelude::*;
use tracing::{info, warn};

use super::types::{ElementFilter, NodeIndex, OsmWay, RawTags, WayMeta};

/// Intermediate way data before geometry resolution.
struct WayData {
    id: i64,
    tags: RawTags,
    node_refs: Vec<i64>,
    meta: WayMeta,
}

/// Read OSM ways matching any of `filters` from a PBF file, together with a [`NodeIndex`]
/// of the referenced nodes (coordinates + per-node use counts for future graph building).
///
/// Fast path (sorted files): exploit the `node → way → relation` ordering to decode the way
/// region first (learning the needed node IDs + counts), then only the node region — each data
/// blob decompressed exactly once, fully parallel. Memory scales with the *selected* network,
/// not the input file size.
///
/// Fallback (unsorted / boundary check fails): the original two-full-pass parallel scan.
///
/// The node coordinates are always built transiently to resolve way geometry. The
/// [`NodeIndex`] (coords + per-node use counts) and the per-way `node_ids` are only *retained*
/// when `find_intersections` is set; otherwise they are dropped right after resolving, keeping
/// peak memory proportional to the way geometry alone.
pub fn read_ways(
    path: &str,
    filters: &[ElementFilter],
    find_intersections: bool,
) -> anyhow::Result<(Vec<OsmWay>, Option<NodeIndex>)> {
    let (ways_data, node_index) = read_ways_indexed_or_fallback(path, filters)?;

    info!("Resolving geometries (parallel)...");
    let ways: Vec<OsmWay> = ways_data
        .into_par_iter()
        .filter_map(|wd| resolve_way(wd, &node_index.coords, find_intersections))
        .collect();
    info!("Resolved: {} ways with valid geometry", ways.len());

    // Drop the node store unless it was requested — frees coords + use_counts before the
    // (long) processing/COPY phase.
    Ok((ways, if find_intersections { Some(node_index) } else { None }))
}

fn read_ways_indexed_or_fallback(
    path: &str,
    filters: &[ElementFilter],
) -> anyhow::Result<(Vec<WayData>, NodeIndex)> {
    info!("Building blob index (no decompression)...");
    let (data_offsets, header_offset) = build_blob_index(path)?;

    // Escape hatch: `PBF_FORCE_FALLBACK=1` skips the ordered fast path (debugging / a file that
    // wrongly advertises Sort.Type_then_ID).
    let force_fallback = std::env::var_os("PBF_FORCE_FALLBACK").is_some();

    let sorted = !force_fallback
        && header_offset
            .map(|off| pbf_is_sorted(path, off).unwrap_or(false))
            .unwrap_or(false);

    let result = if sorted {
        info!(
            "PBF declares Sort.Type_then_ID — using single-pass ordered reader ({} data blobs)",
            data_offsets.len()
        );
        match read_ways_sorted(path, filters, &data_offsets) {
            Ok(r) => Some(r),
            Err(e) => {
                warn!("ordered fast-path failed ({e:#}); falling back to full two-pass scan");
                None
            }
        }
    } else {
        warn!("PBF not declared Sort.Type_then_ID — using full two-pass scan");
        None
    };

    let (ways_data, node_index) = match result {
        Some(r) => r,
        None => read_ways_fallback(path, filters)?,
    };

    let intersections = node_index.use_counts.values().filter(|&&c| c >= 2).count();
    info!(
        "{} kept ways, {} referenced nodes, {} intersection nodes (≥2 ways)",
        ways_data.len(),
        node_index.use_counts.len(),
        intersections
    );
    Ok((ways_data, node_index))
}

// ----------------------------------------------------------------------------------------
// Sorted fast path
// ----------------------------------------------------------------------------------------

/// Single ordered pass: decode the way region, then only the node region.
fn read_ways_sorted(
    path: &str,
    filters: &[ElementFilter],
    data_offsets: &[ByteOffset],
) -> anyhow::Result<(Vec<WayData>, NodeIndex)> {
    let way_start = find_way_section_start(path, data_offsets)?;
    info!(
        "Way region starts at data blob {}/{} (node region: {} blobs)",
        way_start,
        data_offsets.len(),
        way_start
    );

    // Pass A — way region (parallel): collect matching ways.
    let ways_data: Vec<WayData> = data_offsets[way_start..]
        .par_iter()
        .map(|&off| -> anyhow::Result<Vec<WayData>> {
            let block = decode_block(path, off)?;
            let mut out = Vec::new();
            for group in block.groups() {
                for way in group.ways() {
                    if way_passes(filters, &way) {
                        out.push(way_data(&way));
                    }
                }
            }
            Ok(out)
        })
        .collect::<anyhow::Result<Vec<_>>>()?
        .into_iter()
        .flatten()
        .collect();

    // Per-node use counts; the keyset is exactly the needed-node set.
    let use_counts = build_use_counts(&ways_data);

    // Pass B — node region (parallel): coords for needed nodes only.
    let coords = collect_coords(path, &data_offsets[..way_start], &use_counts)?;

    Ok((ways_data, NodeIndex { coords, use_counts }))
}

/// Build the blob index without decompressing any blob: record the byte offset of every
/// `OSMData` blob and the offset of the first `OSMHeader` blob.
fn build_blob_index(path: &str) -> anyhow::Result<(Vec<ByteOffset>, Option<ByteOffset>)> {
    let mut reader = BlobReader::seekable_from_path(path).context("opening PBF for index")?;
    let mut data = Vec::new();
    let mut header = None;

    while let Some(res) = reader.next_header_skip_blob() {
        let (h, off) = res.context("reading blob header")?;
        let off = off.ok_or_else(|| anyhow!("blob header without offset (non-seekable stream?)"))?;
        match h.blob_type() {
            BlobType::OsmData => data.push(off),
            BlobType::OsmHeader => {
                if header.is_none() {
                    header = Some(off);
                }
            }
            BlobType::Unknown(_) => {}
        }
    }

    Ok((data, header))
}

/// True if the PBF header advertises `Sort.Type_then_ID` (nodes, then ways, then relations,
/// each sorted by id). osmium/osmconvert/Geofabrik set this on extracts.
fn pbf_is_sorted(path: &str, header_off: ByteOffset) -> anyhow::Result<bool> {
    let mut reader = BlobReader::seekable_from_path(path).context("opening PBF for header")?;
    let blob = reader.blob_from_offset(header_off).context("reading header blob")?;
    let hb = blob.to_headerblock().context("decoding header blob")?;
    const FLAG: &str = "Sort.Type_then_ID";
    Ok(hb.optional_features().iter().any(|f| f == FLAG)
        || hb.required_features().iter().any(|f| f == FLAG))
}

/// Binary-search the first `OSMData` blob that is **not** node-only (i.e. contains ways or
/// relations). Valid because a sorted file lays out node → way → relation blobs contiguously.
/// A light boundary check guards against a mislabeled file (caller falls back on error).
fn find_way_section_start(path: &str, data: &[ByteOffset]) -> anyhow::Result<usize> {
    let (mut lo, mut hi) = (0usize, data.len());
    while lo < hi {
        let mid = (lo + hi) / 2;
        let (_, has_wr) = blob_kind(&decode_block(path, data[mid])?);
        if has_wr {
            hi = mid;
        } else {
            lo = mid + 1;
        }
    }
    let way_start = lo;

    // Sanity: the boundary must hold (guards a file that lies about its sort order).
    if way_start < data.len() {
        let (_, wr) = blob_kind(&decode_block(path, data[way_start])?);
        if !wr {
            return Err(anyhow!("boundary blob {way_start} has no ways/relations"));
        }
    }
    if way_start > 0 {
        let (nodes, wr) = blob_kind(&decode_block(path, data[way_start - 1])?);
        if wr || !nodes {
            return Err(anyhow!("blob {} before boundary is not node-only", way_start - 1));
        }
    }

    Ok(way_start)
}

/// Returns `(has_nodes, has_ways_or_relations)` for a primitive block.
fn blob_kind(block: &PrimitiveBlock) -> (bool, bool) {
    let mut has_nodes = false;
    let mut has_wr = false;
    for g in block.groups() {
        if g.ways().next().is_some() || g.relations().next().is_some() {
            has_wr = true;
        }
        if g.nodes().next().is_some() || g.dense_nodes().next().is_some() {
            has_nodes = true;
        }
        if has_nodes && has_wr {
            break;
        }
    }
    (has_nodes, has_wr)
}

/// Decode the `OSMData` primitive block at `off`. Opens its own file handle so this is safe
/// to call from parallel rayon tasks.
fn decode_block(path: &str, off: ByteOffset) -> anyhow::Result<PrimitiveBlock> {
    let mut reader = BlobReader::seekable_from_path(path).context("opening PBF for blob")?;
    let blob = reader.blob_from_offset(off).with_context(|| format!("reading blob at {off:?}"))?;
    blob.to_primitiveblock().with_context(|| format!("decoding blob at {off:?}"))
}

/// Collect coordinates (as f32, ~1 m precision) for the needed nodes from the given node-region
/// blobs, in parallel.
fn collect_coords(
    path: &str,
    node_offsets: &[ByteOffset],
    needed: &HashMap<i64, u32>,
) -> anyhow::Result<HashMap<i64, (f32, f32)>> {
    let coords: Vec<(i64, f32, f32)> = node_offsets
        .par_iter()
        .map(|&off| -> anyhow::Result<Vec<(i64, f32, f32)>> {
            let block = decode_block(path, off)?;
            let mut out = Vec::new();
            for group in block.groups() {
                for n in group.dense_nodes() {
                    if needed.contains_key(&n.id()) {
                        out.push((n.id(), n.lon() as f32, n.lat() as f32));
                    }
                }
                for n in group.nodes() {
                    if needed.contains_key(&n.id()) {
                        out.push((n.id(), n.lon() as f32, n.lat() as f32));
                    }
                }
            }
            Ok(out)
        })
        .collect::<anyhow::Result<Vec<_>>>()?
        .into_iter()
        .flatten()
        .collect();

    Ok(coords.into_iter().map(|(id, lon, lat)| (id, (lon, lat))).collect())
}

// ----------------------------------------------------------------------------------------
// Fallback: two full parallel passes (handles unsorted / non-seekable-ordered files)
// ----------------------------------------------------------------------------------------

fn read_ways_fallback(
    path: &str,
    filters: &[ElementFilter],
) -> anyhow::Result<(Vec<WayData>, NodeIndex)> {
    info!("Pass 1 (parallel): collecting matching ways...");
    let ways_data = ElementReader::from_path(path)
        .context("opening PBF for pass 1")?
        .par_map_reduce(
            |element| {
                let mut ways = Vec::new();
                if let Element::Way(way) = element {
                    if way_passes(filters, &way) {
                        ways.push(way_data(&way));
                    }
                }
                ways
            },
            Vec::new,
            |mut a, b| {
                a.extend(b);
                a
            },
        )
        .context("pass 1 parallel read")?;

    let use_counts = build_use_counts(&ways_data);

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

    let coords = coords_vec.into_iter().map(|(id, lon, lat)| (id, (lon, lat))).collect();
    Ok((ways_data, NodeIndex { coords, use_counts }))
}

// ----------------------------------------------------------------------------------------
// Shared helpers
// ----------------------------------------------------------------------------------------

/// True if the way matches any topic's element filter.
fn way_passes(filters: &[ElementFilter], way: &Way) -> bool {
    filters.iter().any(|f| {
        way.tags().any(|(k, v)| {
            k == f.tag
                && match &f.any_of {
                    None => true,
                    Some(allowed) => allowed.iter().any(|a| a == v),
                }
        })
    })
}

/// Extract a [`WayData`] from an osmpbf `Way` (shared by the indexed and fallback readers).
fn way_data(way: &Way) -> WayData {
    let tags: RawTags = way.tags().map(|(k, v)| (k.to_owned(), v.to_owned())).collect();
    let refs: Vec<i64> = way.refs().collect();

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

    WayData { id: way.id(), tags, node_refs: refs, meta }
}

/// Per-node use count across all kept ways. `>= 2` ⇒ a node shared by multiple ways
/// (an intersection / crossing) — the seed of a routable graph.
fn build_use_counts(ways: &[WayData]) -> HashMap<i64, u32> {
    let mut counts: HashMap<i64, u32> = HashMap::new();
    for w in ways {
        for &id in &w.node_refs {
            *counts.entry(id).or_insert(0) += 1;
        }
    }
    counts
}

/// Resolve a `WayData` into an `OsmWay` by looking up node coordinates. `keep_node_ids`
/// retains the referenced node ids for graph building; otherwise they are dropped to save memory.
fn resolve_way(
    wd: WayData,
    coords: &HashMap<i64, (f32, f32)>,
    keep_node_ids: bool,
) -> Option<OsmWay> {
    let pts: Vec<(f64, f64)> = wd
        .node_refs
        .iter()
        .filter_map(|id| coords.get(id))
        .map(|&(lon, lat)| (lon as f64, lat as f64))
        .collect();

    if pts.len() < 2 {
        return None;
    }

    let node_ids = if keep_node_ids { wd.node_refs } else { Vec::new() };
    Some(OsmWay { id: wd.id, coords: pts, node_ids, tags: wd.tags, meta: wd.meta })
}
