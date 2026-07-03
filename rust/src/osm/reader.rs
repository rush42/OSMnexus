use anyhow::{anyhow, Context};
use rustc_hash::FxHashMap;
use osmpbf::{BlobReader, BlobType, ByteOffset, Element, ElementReader, PrimitiveBlock, Way};
use rayon::prelude::*;
use tracing::{info, warn};

use super::types::{ElementFilter, OsmWay, RawTags, WayData, WayMeta};

/// A compact, allocation-frugal store of the kept ways' node-id lists, built in Pass A and consumed
/// by the geometry pass. CSR layout: one flat `refs` vector holds every kept way's node ids
/// back-to-back; `ways` indexes into it as `(id, start, len, payload)`. Avoids one `Vec` per way.
struct WayIndex<M> {
    refs: Vec<i64>,
    ways: Vec<(i64, u32, u32, M)>,
}

impl<M> WayIndex<M> {
    fn new() -> Self {
        WayIndex { refs: Vec::new(), ways: Vec::new() }
    }
    fn push(&mut self, id: i64, node_refs: &[i64], payload: M) {
        let start = self.refs.len() as u32;
        self.refs.extend_from_slice(node_refs);
        self.ways.push((id, start, node_refs.len() as u32, payload));
    }
}

/// Stream OSM ways from a PBF, in two topic-agnostic callbacks:
///   * `classify(&WayData)` runs in Pass A (tag-only, geometry-free) and returns `Some(payload)` for
///     a kept way or `None` for a fully pruned one. Its side effect is emitting the tag rows.
///   * `build_geom(&OsmWay, payload)` runs in the geometry pass on each kept way once its geometry is
///     resolved, emitting the geom rows.
/// The `payload: M` is opaque to the reader — the caller uses it to carry "which topics kept this
/// way" from classify through to build_geom.
///
/// Fast path (sorted `node → way → relation` files):
///   * Pass A — decode the way region **once**: filter, tally per-node use counts, classify (emit
///     tags), and record each kept way's node ids in a `WayIndex`.
///   * Pass B — decode the node region once, collect coords for the needed nodes.
///   * Geometry pass — resolve each indexed way against the coords/use-counts and call `build_geom`.
///     **No second way-region decode**; peak memory adds the kept ways' node-id lists.
///
/// Fallback (unsorted / boundary check fails / `PBF_FORCE_FALLBACK`): three full parallel scans
/// (refs → coords → classify+geometry). Rare; re-decodes the way region in pass 3 rather than
/// holding the node-id index, but is otherwise behaviorally identical.
pub fn stream_ways<C, G, M>(
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
    info!("Building blob index (no decompression)...");
    let t_idx = std::time::Instant::now();
    let (data_offsets, header_offset) = build_blob_index(path)?;
    info!("[phase] blob index build: {:.1}s", t_idx.elapsed().as_secs_f32());

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

                // Pass A — way region (decoded once): filter + counts + classify (emit tags) + index.
                let t = std::time::Instant::now();
                let (use_counts, index) = classify_and_index(path, way_offsets, filters, &classify)?;
                info!("[phase] Pass A (filter + classify + emit tags): {:.1}s", t.elapsed().as_secs_f32());
                // Pass B — node region: coords for needed nodes only.
                let t = std::time::Instant::now();
                let coords = collect_coords(path, node_offsets, &use_counts)?;
                info!("[phase] Pass B (collect node coords): {:.1}s", t.elapsed().as_secs_f32());
                log_node_summary(&use_counts);
                // Geometry pass — resolve each indexed way + emit geom. No blob decode here; the
                // node maps + index drop afterwards, freeing memory before index creation. Overlaps
                // with the DB-drain consumer, so its duration is the *producer* side only.
                let t = std::time::Instant::now();
                build_geometries(&index, &coords, &use_counts, &build_geom)?;
                info!("[phase] Geometry pass (resolve + build geometry + emit geom, no decode): {:.1}s", t.elapsed().as_secs_f32());

                return Ok(());
            }
            Err(e) => {
                warn!("ordered fast-path boundary check failed ({e:#}); falling back to full scan");
            }
        }
    } else {
        warn!("PBF not declared Sort.Type_then_ID — using full-scan streaming reader");
    }

    stream_ways_fallback(path, filters, classify, build_geom)
}

// ----------------------------------------------------------------------------------------
// Sorted fast path
// ----------------------------------------------------------------------------------------

/// Pass A — decode the way-region blobs once (parallel). For every filter-passing way, tally its
/// node refs into the global use-count map (intersection detection spans all filter-passing ways,
/// not just classified-kept ones), classify it (side effect: emit tag rows), and — when kept —
/// record its node ids in the `WayIndex` for the geometry pass. Tags/meta drop after classify.
fn classify_and_index<C, M>(
    path: &str,
    way_offsets: &[ByteOffset],
    filters: &[ElementFilter],
    classify: &C,
) -> anyhow::Result<(FxHashMap<i64, u32>, WayIndex<M>)>
where
    C: Fn(&WayData) -> Option<M> + Sync,
    M: Copy + Send + Sync,
{
    use crate::profile::{self, DECODE, TAGBUILD};

    // Each blob independently: all filter-passing refs (for counts) + a local kept index segment.
    let per_blob: Vec<(Vec<i64>, WayIndex<M>)> = way_offsets
        .par_iter()
        .map(|&off| -> anyhow::Result<(Vec<i64>, WayIndex<M>)> {
            let block = profile::time(&DECODE, || decode_block(path, off))?;
            let mut count_refs: Vec<i64> = Vec::new();
            let mut seg = WayIndex::new();
            for group in block.groups() {
                for way in group.ways() {
                    if way_passes(filters, &way) {
                        let wd = profile::time(&TAGBUILD, || way_data(&way));
                        count_refs.extend_from_slice(&wd.node_refs);
                        if let Some(m) = classify(&wd) {
                            seg.push(wd.id, &wd.node_refs, m);
                        }
                    }
                }
            }
            Ok((count_refs, seg))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;

    // Merge: sum counts, and concatenate the per-blob index segments (rebasing their offsets).
    let mut counts: FxHashMap<i64, u32> = FxHashMap::default();
    let mut index: WayIndex<M> = WayIndex::new();
    for (count_refs, seg) in per_blob {
        for id in count_refs {
            *counts.entry(id).or_insert(0) += 1;
        }
        let base = index.refs.len() as u32;
        index.refs.extend_from_slice(&seg.refs);
        for (id, start, len, m) in seg.ways {
            index.ways.push((id, base + start, len, m));
        }
    }
    Ok((counts, index))
}

/// Geometry pass — resolve each indexed way against the coords map (parallel) and hand it to
/// `build_geom`. No blob decode: the ways' node ids come straight from the in-memory `WayIndex`.
fn build_geometries<G, M>(
    index: &WayIndex<M>,
    coords: &FxHashMap<i64, (f32, f32)>,
    use_counts: &FxHashMap<i64, u32>,
    build_geom: &G,
) -> anyhow::Result<()>
where
    G: Fn(&OsmWay, M) + Sync,
    M: Copy + Sync,
{
    use crate::profile::{self, RESOLVE};
    index.ways.par_iter().try_for_each(|&(id, start, len, m)| -> anyhow::Result<()> {
        let refs = &index.refs[start as usize..(start + len) as usize];
        if let Some(w) = profile::time(&RESOLVE, || resolve_geometry(id, refs, coords, use_counts)) {
            build_geom(&w, m);
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
    needed: &FxHashMap<i64, u32>,
) -> anyhow::Result<FxHashMap<i64, (f32, f32)>> {
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

fn stream_ways_fallback<C, G, M>(
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
    let coords: FxHashMap<i64, (f32, f32)> =
        coords_vec.into_iter().map(|(id, lon, lat)| (id, (lon, lat))).collect();

    log_node_summary(&use_counts);

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
                            if let Some(w) =
                                resolve_geometry(wd.id, &wd.node_refs, &coords, &use_counts)
                            {
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

// ----------------------------------------------------------------------------------------
// Shared helpers
// ----------------------------------------------------------------------------------------

fn log_node_summary(use_counts: &FxHashMap<i64, u32>) {
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

/// Resolve a way's node ids into an `OsmWay` geometry by looking up node coordinates. Tag/meta are
/// not involved — classification already ran in Pass A.
fn resolve_geometry(
    id: i64,
    node_refs: &[i64],
    coords: &FxHashMap<i64, (f32, f32)>,
    use_counts: &FxHashMap<i64, u32>,
) -> Option<OsmWay> {
    // One pass: keep only nodes that have coords, tracking their ids so cut points stay aligned
    // to `pts` indices (a dropped missing-coord node must not shift the indices).
    let mut pts: Vec<(f64, f64)> = Vec::with_capacity(node_refs.len());
    let mut kept_ids: Vec<i64> = Vec::with_capacity(node_refs.len());
    for &id in node_refs {
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
    for (i, &nid) in kept_ids.iter().enumerate() {
        if i == 0 || i == last || use_counts.get(&nid).copied().unwrap_or(0) > 1 {
            cut_points.push((i as u32, nid));
        }
    }

    Some(OsmWay { id, coords: pts, cut_points })
}
