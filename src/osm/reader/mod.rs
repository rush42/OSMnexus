//! Topic-agnostic PBF streaming reader. Drives the classification of relations, ways, and nodes,
//! plus way geometry, over a PBF. The sorted fast path decodes each blob region once (in the
//! logical order relations → ways → nodes → geometry); an unsorted file falls back to full scans.

mod blob_index;
mod fallback;
mod resolve;
mod sorted;

use tracing::{info, warn};

use crate::osm::types::{NodeData, OsmWay, RelData, WayData};

use blob_index::{build_blob_index, find_relation_section_start, find_way_section_start, pbf_is_sorted};
use fallback::stream_osm_fallback;
use resolve::log_node_summary;
use sorted::{build_geometries, classify_and_index, classify_relations, collect_coords};

/// The topic-agnostic callbacks driving one PBF read. `M` is an opaque per-way payload the caller
/// uses to carry "which topics kept this way" from classify through to build_geom.
pub struct Callbacks<CR, CW, CN, G> {
    /// Whether any topic declares relation categories — gates the relations pass entirely.
    pub has_relations: bool,
    /// Relations pass: emit relation tag rows + `relation_members`; return `true` if the relation
    /// was kept (its member ways then join the keep set).
    pub classify_rel: CR,
    /// Ways pass: emit tag rows; return `Some(payload)` for a kept way (produced ≥1 row **or** the
    /// `bool` — relation membership — is true), `None` for a fully pruned way.
    pub classify_way: CW,
    /// Whether any topic declares node categories — gates node tag decoding in Pass B.
    pub has_nodes: bool,
    /// Nodes pass: emit node tag rows; return `true` if the node was selected (a forced cut point).
    pub classify_node: CN,
    /// Geometry pass: emit the geom rows for a resolved way.
    pub build_geom: G,
}

/// Stream an OSM PBF, classifying relations, ways, and nodes and resolving way geometry.
///
/// Fast path (sorted `node → way → relation` files) runs, in logical order:
///   * Relations pass — decode the relation region once: classify relations, collect the member-way
///     keep set.
///   * Pass A — decode the way region once: classify ways (keeping relation members too), tally
///     per-node use counts, index each kept way's node ids.
///   * Pass B — decode the node region once: collect coords for needed nodes and (if node topics
///     exist) classify node tags, collecting the selected-node set.
///   * Geometry pass — resolve each indexed way, selected nodes forced as cut points.
///
/// Fallback (unsorted / boundary check fails / `PBF_FORCE_FALLBACK`): full parallel scans.
pub fn stream_osm<CR, CW, CN, G, M>(
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
            "PBF declares Sort.Type_then_ID — ordered streaming reader ({} data blobs)",
            data_offsets.len()
        );
        // The only "risky" steps (the sort-order assumptions) are the boundary searches; they run
        // before any streaming, so a failure can still fall back cleanly.
        match find_regions(path, &data_offsets) {
            Ok((way_start, rel_start)) => {
                let node_offsets = &data_offsets[..way_start];
                let way_offsets = &data_offsets[way_start..rel_start];
                let rel_offsets = &data_offsets[rel_start..];
                info!(
                    "Regions — nodes: {} blobs, ways: {} blobs, relations: {} blobs",
                    node_offsets.len(),
                    way_offsets.len(),
                    rel_offsets.len()
                );

                // Relations pass — collect the member-way keep set (+ emit relation rows).
                let keep_set = if cb.has_relations && !rel_offsets.is_empty() {
                    let t = std::time::Instant::now();
                    let ks = classify_relations(path, rel_offsets, &cb.classify_rel)?;
                    info!(
                        "[phase] Relations pass (classify + emit): {:.1}s ({} member ways kept)",
                        t.elapsed().as_secs_f32(),
                        ks.len()
                    );
                    ks
                } else {
                    rustc_hash::FxHashSet::default()
                };

                // Pass A — way region (decoded once): classify + counts + index.
                let t = std::time::Instant::now();
                let (use_counts, index) = classify_and_index(path, way_offsets, &keep_set, &cb.classify_way)?;
                info!("[phase] Pass A (classify ways + emit tags): {:.1}s", t.elapsed().as_secs_f32());
                drop(keep_set);

                // Pass B — node region: coords for needed nodes (+ classify nodes).
                let t = std::time::Instant::now();
                let (coords, selected) =
                    collect_coords(path, node_offsets, &use_counts, cb.has_nodes, &cb.classify_node)?;
                info!(
                    "[phase] Pass B (collect node coords{}): {:.1}s",
                    if cb.has_nodes { " + classify nodes" } else { "" },
                    t.elapsed().as_secs_f32()
                );
                log_node_summary(&use_counts);
                drop(use_counts);

                // Geometry pass — resolve each indexed way + emit geom. No blob decode here.
                let t = std::time::Instant::now();
                build_geometries(&index, &coords, &selected, &cb.build_geom)?;
                info!("[phase] Geometry pass (resolve + build geometry, no decode): {:.1}s", t.elapsed().as_secs_f32());

                return Ok(());
            }
            Err(e) => {
                warn!("ordered fast-path boundary check failed ({e:#}); falling back to full scan");
            }
        }
    } else {
        warn!("PBF not declared Sort.Type_then_ID — using full-scan streaming reader");
    }

    stream_osm_fallback(path, cb)
}

/// Locate the `(way_start, rel_start)` region boundaries in a sorted file's blob list.
fn find_regions(path: &str, data: &[osmpbf::ByteOffset]) -> anyhow::Result<(usize, usize)> {
    let way_start = find_way_section_start(path, data)?;
    let rel_start = find_relation_section_start(path, data, way_start)?;
    Ok((way_start, rel_start))
}
