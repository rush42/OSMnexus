//! The sorted fast path: Pass A (filter + classify + index), Pass B (collect node coords), and the
//! geometry pass (resolve indexed ways) — each decoding its blob region once.

use rustc_hash::FxHashMap;
use osmpbf::ByteOffset;
use rayon::prelude::*;

use crate::osm::types::{ElementFilter, OsmWay, WayData};

use super::blob_index::decode_block;
use super::resolve::{resolve_geometry, way_data, way_passes, NodeCoords};

/// A compact, allocation-frugal store of the kept ways' node-id lists, built in Pass A and consumed
/// by the geometry pass. CSR layout: one flat `refs` vector holds every kept way's node ids
/// back-to-back; `ways` indexes into it as `(id, start, len, payload)`. Avoids one `Vec` per way.
pub(super) struct WayIndex<M> {
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

/// Merge two `(use_counts, WayIndex)` accumulators: sum the count maps (folding the smaller into
/// the larger to minimise inserts) and concatenate the index segments (rebasing `b`'s ref offsets
/// onto the end of `a`'s ref buffer). Used as the `try_reduce` combiner in Pass A.
fn merge_acc<M>(
    a: (FxHashMap<i64, u32>, WayIndex<M>),
    b: (FxHashMap<i64, u32>, WayIndex<M>),
) -> (FxHashMap<i64, u32>, WayIndex<M>) {
    let (ca, ia) = a;
    let (cb, ib) = b;

    let mut counts = if ca.len() >= cb.len() { (ca, cb) } else { (cb, ca) };
    let (big, small) = (&mut counts.0, counts.1);
    for (k, v) in small {
        *big.entry(k).or_insert(0) += v;
    }
    let counts = counts.0;

    let mut index = ia;
    let base = index.refs.len() as u32;
    index.refs.extend_from_slice(&ib.refs);
    for (id, start, len, m) in ib.ways {
        index.ways.push((id, base + start, len, m));
    }
    (counts, index)
}

/// Pass A — decode the way-region blobs once (parallel). For every filter-passing way, tally its
/// node refs into the use-count map (intersection detection spans all filter-passing ways, not just
/// classified-kept ones), classify it (side effect: emit tag rows), and — when kept — record its
/// node ids in the `WayIndex` for the geometry pass. Tags/meta drop after classify.
///
/// Counts are folded into per-blob maps and merged via `try_reduce`, so the raw node refs are never
/// materialised in bulk (avoids a multi-GB transient on large imports).
pub(super) fn classify_and_index<C, M>(
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

    way_offsets
        .par_iter()
        .map(|&off| -> anyhow::Result<(FxHashMap<i64, u32>, WayIndex<M>)> {
            let block = profile::time(&DECODE, || decode_block(path, off))?;
            let mut counts: FxHashMap<i64, u32> = FxHashMap::default();
            let mut seg = WayIndex::new();
            for group in block.groups() {
                for way in group.ways() {
                    if way_passes(filters, &way) {
                        let wd = profile::time(&TAGBUILD, || way_data(&way));
                        for &id in &wd.node_refs {
                            *counts.entry(id).or_insert(0) += 1;
                        }
                        if let Some(m) = classify(&wd) {
                            seg.push(wd.id, &wd.node_refs, m);
                        }
                    }
                }
            }
            Ok((counts, seg))
        })
        .try_reduce(|| (FxHashMap::default(), WayIndex::new()), |a, b| Ok(merge_acc(a, b)))
}

/// Collect coordinates (as f32, ~1 m precision) for the needed nodes from the given node-region
/// blobs, in parallel. Each node's `shared` flag (used by ≥2 ways) is read from `use_counts` here
/// and baked into the value, so the caller can drop `use_counts` before the geometry pass.
pub(super) fn collect_coords(
    path: &str,
    node_offsets: &[ByteOffset],
    use_counts: &FxHashMap<i64, u32>,
) -> anyhow::Result<NodeCoords> {
    let coords: Vec<(i64, f32, f32, bool)> = node_offsets
        .par_iter()
        .map(|&off| -> anyhow::Result<Vec<(i64, f32, f32, bool)>> {
            let block = decode_block(path, off)?;
            let mut out = Vec::new();
            let mut take = |id: i64, lon: f32, lat: f32| {
                if let Some(&c) = use_counts.get(&id) {
                    out.push((id, lon, lat, c > 1));
                }
            };
            for group in block.groups() {
                for n in group.dense_nodes() {
                    take(n.id(), n.lon() as f32, n.lat() as f32);
                }
                for n in group.nodes() {
                    take(n.id(), n.lon() as f32, n.lat() as f32);
                }
            }
            Ok(out)
        })
        .collect::<anyhow::Result<Vec<_>>>()?
        .into_iter()
        .flatten()
        .collect();

    Ok(coords.into_iter().map(|(id, lon, lat, shared)| (id, (lon, lat, shared))).collect())
}

/// Geometry pass — resolve each indexed way against the coords map (parallel) and hand it to
/// `build_geom`. No blob decode: the ways' node ids come straight from the in-memory `WayIndex`.
pub(super) fn build_geometries<G, M>(
    index: &WayIndex<M>,
    coords: &NodeCoords,
    build_geom: &G,
) -> anyhow::Result<()>
where
    G: Fn(&OsmWay, M) + Sync,
    M: Copy + Sync,
{
    use crate::profile::{self, RESOLVE};
    index.ways.par_iter().try_for_each(|&(id, start, len, m)| -> anyhow::Result<()> {
        let refs = &index.refs[start as usize..(start + len) as usize];
        if let Some(w) = profile::time(&RESOLVE, || resolve_geometry(id, refs, coords)) {
            build_geom(&w, m);
        }
        Ok(())
    })
}
