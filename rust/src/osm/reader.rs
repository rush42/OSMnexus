use std::collections::HashMap;

use anyhow::{anyhow, Context};
use osmpbf::{BlobReader, BlobType, ByteOffset, Element, ElementReader, PrimitiveBlock, Way};
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

/// Stream OSM ways matching any of `filters` from a PBF file, invoking `for_each` on each
/// resolved way. Returns a [`NodeIndex`] (coords + per-node use counts) when
/// `find_intersections` is set, else `None`.
///
/// Ways are **streamed, not materialized**: each way is resolved, handed to `for_each`, and
/// dropped — so peak memory is proportional to the referenced-node coordinates (the selected
/// network), not the number of ways or their tags.
///
/// Fast path (sorted `node → way → relation` files):
///   * Pass A — decode the way region, collect per-node use counts (= the needed-node set).
///   * Pass B — decode the node region once, collect coords for those nodes.
///   * Pass C — decode the way region again, resolve + `for_each` each way, streaming.
///
/// The way region (small — typically ~15% of blobs) is decoded twice; the heavy node region
/// once. This trades ~14% extra decode for bounded, network-proportional memory.
///
/// Fallback (unsorted / boundary check fails / `PBF_FORCE_FALLBACK`): three full parallel
/// scans (refs → coords → stream). Rare; still streams, still bounded memory.
pub fn stream_ways<F>(
    path: &str,
    filters: &[ElementFilter],
    find_intersections: bool,
    for_each: F,
) -> anyhow::Result<Option<NodeIndex>>
where
    F: Fn(&OsmWay) + Sync + Send,
{
    info!("Building blob index (no decompression)...");
    let (data_offsets, header_offset) = build_blob_index(path)?;

    // Escape hatch: `PBF_FORCE_FALLBACK=1` skips the ordered fast path (debugging / a file that
    // wrongly advertises Sort.Type_then_ID).
    let force_fallback = std::env::var_os("PBF_FORCE_FALLBACK").is_some();
    let sorted = !force_fallback
        && header_offset
            .map(|off| pbf_is_sorted(path, off).unwrap_or(false))
            .unwrap_or(false);

    if sorted {
        info!(
            "PBF declares Sort.Type_then_ID — single-pass ordered streaming reader ({} data blobs)",
            data_offsets.len()
        );
        // The only "risky" step (the sort-order assumption) is the boundary search; it runs
        // before any streaming, so a failure can still fall back cleanly.
        match find_way_section_start(path, &data_offsets) {
            Ok(way_start) => {
                info!(
                    "Way region starts at data blob {}/{} (node region: {} blobs)",
                    way_start,
                    data_offsets.len(),
                    way_start
                );
                let way_offsets = &data_offsets[way_start..];
                let node_offsets = &data_offsets[..way_start];

                // Pass A — way region: per-node use counts (keyset = needed-node set).
                let use_counts = collect_use_counts(path, way_offsets, filters)?;
                // Pass B — node region: coords for needed nodes only.
                let coords = collect_coords(path, node_offsets, &use_counts)?;
                log_node_summary(&use_counts);
                // Pass C — way region again: resolve + stream each way.
                stream_way_region(path, way_offsets, filters, &coords, &use_counts, &for_each)?;

                return Ok(find_intersections.then_some(NodeIndex { coords, use_counts }));
            }
            Err(e) => {
                warn!("ordered fast-path boundary check failed ({e:#}); falling back to full scan");
            }
        }
    } else {
        warn!("PBF not declared Sort.Type_then_ID — using full-scan streaming reader");
    }

    stream_ways_fallback(path, filters, find_intersections, for_each)
}

// ----------------------------------------------------------------------------------------
// Sorted fast path
// ----------------------------------------------------------------------------------------

/// Pass A — decode the way-region blobs (parallel) and tally per-node use counts across all
/// matching ways. The count map's keyset is exactly the needed-node set for Pass B.
fn collect_use_counts(
    path: &str,
    way_offsets: &[ByteOffset],
    filters: &[ElementFilter],
) -> anyhow::Result<HashMap<i64, u32>> {
    let per_blob: Vec<Vec<i64>> = way_offsets
        .par_iter()
        .map(|&off| -> anyhow::Result<Vec<i64>> {
            let block = decode_block(path, off)?;
            let mut refs = Vec::new();
            for group in block.groups() {
                for way in group.ways() {
                    if way_passes(filters, &way) {
                        refs.extend(way.refs());
                    }
                }
            }
            Ok(refs)
        })
        .collect::<anyhow::Result<Vec<_>>>()?;

    let mut counts: HashMap<i64, u32> = HashMap::new();
    for refs in per_blob {
        for id in refs {
            *counts.entry(id).or_insert(0) += 1;
        }
    }
    Ok(counts)
}

/// Pass C — decode the way-region blobs again (parallel), resolve each matching way against the
/// coords map, and stream it to `for_each`. Nothing accumulates; ways are dropped after the call.
fn stream_way_region<F>(
    path: &str,
    way_offsets: &[ByteOffset],
    filters: &[ElementFilter],
    coords: &HashMap<i64, (f32, f32)>,
    use_counts: &HashMap<i64, u32>,
    for_each: &F,
) -> anyhow::Result<()>
where
    F: Fn(&OsmWay) + Sync,
{
    way_offsets.par_iter().try_for_each(|&off| -> anyhow::Result<()> {
        let block = decode_block(path, off)?;
        for group in block.groups() {
            for way in group.ways() {
                if way_passes(filters, &way) {
                    if let Some(w) = resolve_way(way_data(&way), coords, use_counts) {
                        for_each(&w);
                    }
                }
            }
        }
        Ok(())
    })
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
// Fallback: full parallel scans (handles unsorted / non-seekable-ordered files)
// ----------------------------------------------------------------------------------------

fn stream_ways_fallback<F>(
    path: &str,
    filters: &[ElementFilter],
    find_intersections: bool,
    for_each: F,
) -> anyhow::Result<Option<NodeIndex>>
where
    F: Fn(&OsmWay) + Sync + Send,
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

    let mut use_counts: HashMap<i64, u32> = HashMap::new();
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
    let coords: HashMap<i64, (f32, f32)> =
        coords_vec.into_iter().map(|(id, lon, lat)| (id, (lon, lat))).collect();

    log_node_summary(&use_counts);

    info!("Pass 3 (parallel): streaming ways...");
    ElementReader::from_path(path)
        .context("opening PBF for pass 3")?
        .par_map_reduce(
            |element| {
                if let Element::Way(way) = element {
                    if way_passes(filters, &way) {
                        if let Some(w) = resolve_way(way_data(&way), &coords, &use_counts) {
                            for_each(&w);
                        }
                    }
                }
            },
            || (),
            |_, _| (),
        )
        .context("pass 3 parallel read")?;

    Ok(find_intersections.then_some(NodeIndex { coords, use_counts }))
}

// ----------------------------------------------------------------------------------------
// Shared helpers
// ----------------------------------------------------------------------------------------

fn log_node_summary(use_counts: &HashMap<i64, u32>) {
    let intersections = use_counts.values().filter(|&&c| c >= 2).count();
    info!(
        "{} referenced nodes, {} intersection nodes (≥2 ways)",
        use_counts.len(),
        intersections
    );
}

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

/// Extract a [`WayData`] from an osmpbf `Way`.
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

/// Resolve a `WayData` into an `OsmWay` by looking up node coordinates.
fn resolve_way(
    wd: WayData,
    coords: &HashMap<i64, (f32, f32)>,
    use_counts: &HashMap<i64, u32>,
) -> Option<OsmWay> {
    // One pass: keep only nodes that have coords, tracking their ids so cut points stay aligned
    // to `pts` indices (a dropped missing-coord node must not shift the indices).
    let mut pts: Vec<(f64, f64)> = Vec::with_capacity(wd.node_refs.len());
    let mut kept_ids: Vec<i64> = Vec::with_capacity(wd.node_refs.len());
    for &id in &wd.node_refs {
        if let Some(&(lon, lat)) = coords.get(&id) {
            pts.push((lon as f64, lat as f64));
            kept_ids.push(id);
        }
    }

    if pts.len() < 2 {
        return None;
    }

    // Cut points: start + end (always), plus interior nodes shared with another way (count > 1).
    let last = kept_ids.len() - 1;
    let mut cut_points: Vec<(u32, i64)> = Vec::new();
    for (i, &id) in kept_ids.iter().enumerate() {
        if i == 0 || i == last || use_counts.get(&id).copied().unwrap_or(0) > 1 {
            cut_points.push((i as u32, id));
        }
    }

    Some(OsmWay { id: wd.id, coords: pts, cut_points, tags: wd.tags, meta: wd.meta })
}
