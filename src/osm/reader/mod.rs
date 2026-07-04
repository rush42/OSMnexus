//! Topic-agnostic PBF streaming reader. Drives two callbacks over the ways of a PBF: `classify`
//! (Pass A, tag-only) and `build_geom` (geometry pass). The sorted fast path decodes each blob
//! region once; an unsorted file falls back to three full scans. See the submodules for the two
//! paths and their shared leaf helpers.

mod blob_index;
mod fallback;
mod resolve;
mod sorted;

use tracing::{info, warn};

use crate::osm::types::{ElementFilter, OsmWay, WayData};

use blob_index::{build_blob_index, find_way_section_start, pbf_is_sorted};
use fallback::stream_ways_fallback;
use resolve::log_node_summary;
use sorted::{build_geometries, classify_and_index, collect_coords};

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
                // `use_counts` is no longer needed — the shared flag is baked into `coords` — so drop
                // it before the geometry pass, which then holds only `coords` + the way index.
                drop(use_counts);
                // Geometry pass — resolve each indexed way + emit geom. No blob decode here; the
                // coords map + index drop afterwards, freeing memory before index creation. Overlaps
                // with the DB-drain consumer, so its duration is the *producer* side only.
                let t = std::time::Instant::now();
                build_geometries(&index, &coords, &build_geom)?;
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
