//! The sorted fast path: the relations pass (classify + collect member-way keep set), Pass A
//! (classify ways + index), Pass B (collect node coords + classify nodes), and the geometry pass
//! (resolve indexed ways) — each decoding its blob region once.

use rustc_hash::{FxHashMap, FxHashSet};
use osmpbf::ByteOffset;
use rayon::prelude::*;

use crate::osm::types::{NodeData, OsmWay, RelData, WayData};

use super::blob_index::decode_block;
use super::resolve::{
    dense_node_data, node_data, rel_data, resolve_geometry, way_data, NodeCoords,
};

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

/// Relations pass — decode the relation region once (parallel). For every relation, extract its
/// `RelData` and run `classify_rel` (side effect: emit relation tag rows + `relation_members`
/// links). When `classify_rel` returns `true` (the relation was kept), its member way ids are added
/// to the keep set so those ways survive Pass A even if their own tags classify to nothing.
pub(super) fn classify_relations<CR>(
    path: &str,
    rel_offsets: &[ByteOffset],
    classify_rel: &CR,
) -> anyhow::Result<FxHashSet<i64>>
where
    CR: Fn(&RelData) -> bool + Sync,
{
    rel_offsets
        .par_iter()
        .map(|&off| -> anyhow::Result<FxHashSet<i64>> {
            let block = decode_block(path, off)?;
            let mut keep: FxHashSet<i64> = FxHashSet::default();
            for group in block.groups() {
                for rel in group.relations() {
                    let rd = rel_data(&rel);
                    if classify_rel(&rd) {
                        keep.extend(rd.member_ways.iter().copied());
                    }
                }
            }
            Ok(keep)
        })
        .try_reduce(FxHashSet::default, |mut a, b| {
            a.extend(b);
            Ok(a)
        })
}

/// Pass A — decode the way-region blobs once (parallel). For every way, extract its `WayData` and
/// classify it (side effect: emit tag rows); a way is *kept* when `classify` returns `Some` — it
/// produced ≥1 topic row **or** is a relation member (`keep_set`). Kept ways have their node refs
/// tallied into the use-count map (intersection detection) and their node ids recorded in the
/// `WayIndex` for the geometry pass. Tags/meta drop after classify.
pub(super) fn classify_and_index<C, M>(
    path: &str,
    way_offsets: &[ByteOffset],
    keep_set: &FxHashSet<i64>,
    classify: &C,
) -> anyhow::Result<(FxHashMap<i64, u32>, WayIndex<M>)>
where
    C: Fn(&WayData, bool) -> Option<M> + Sync,
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
                    let wd = profile::time(&TAGBUILD, || way_data(&way));
                    let in_keep = keep_set.contains(&wd.id);
                    if let Some(m) = classify(&wd, in_keep) {
                        for &id in &wd.node_refs {
                            *counts.entry(id).or_insert(0) += 1;
                        }
                        seg.push(wd.id, &wd.node_refs, m);
                    }
                }
            }
            Ok((counts, seg))
        })
        .try_reduce(|| (FxHashMap::default(), WayIndex::new()), |a, b| Ok(merge_acc(a, b)))
}

/// Pass B — collect coordinates (as f32, ~1 m precision) for the needed nodes from the node-region
/// blobs, in parallel. Each node's `shared` flag (used by ≥2 ways) is read from `use_counts` here
/// and baked into the value, so the caller can drop `use_counts` before the geometry pass.
///
/// When `classify_nodes` is set, every needed node's tags are also decoded and passed to
/// `classify_node` (side effect: emit node tag rows); a node it returns `true` for is *selected*
/// and its id is accumulated into the returned set (forced graph-vertex cut points). When unset,
/// node tags are not decoded at all — preserving the way-only performance profile.
pub(super) fn collect_coords<CN>(
    path: &str,
    node_offsets: &[ByteOffset],
    use_counts: &FxHashMap<i64, u32>,
    classify_nodes: bool,
    classify_node: &CN,
) -> anyhow::Result<(NodeCoords, FxHashSet<i64>)>
where
    CN: Fn(&NodeData) -> bool + Sync,
{
    let per_blob: Vec<(Vec<(i64, f32, f32, bool)>, FxHashSet<i64>)> = node_offsets
        .par_iter()
        .map(|&off| -> anyhow::Result<(Vec<(i64, f32, f32, bool)>, FxHashSet<i64>)> {
            let block = decode_block(path, off)?;
            let mut out = Vec::new();
            let mut selected: FxHashSet<i64> = FxHashSet::default();
            for group in block.groups() {
                for n in group.dense_nodes() {
                    if let Some(&c) = use_counts.get(&n.id()) {
                        out.push((n.id(), n.lon() as f32, n.lat() as f32, c > 1));
                        if classify_nodes && classify_node(&dense_node_data(&n)) {
                            selected.insert(n.id());
                        }
                    }
                }
                for n in group.nodes() {
                    if let Some(&c) = use_counts.get(&n.id()) {
                        out.push((n.id(), n.lon() as f32, n.lat() as f32, c > 1));
                        if classify_nodes && classify_node(&node_data(&n)) {
                            selected.insert(n.id());
                        }
                    }
                }
            }
            Ok((out, selected))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;

    let mut coords: NodeCoords = FxHashMap::default();
    let mut selected: FxHashSet<i64> = FxHashSet::default();
    for (chunk, sel) in per_blob {
        for (id, lon, lat, shared) in chunk {
            coords.insert(id, (lon, lat, shared));
        }
        selected.extend(sel);
    }
    Ok((coords, selected))
}

/// Geometry pass — resolve each indexed way against the coords map (parallel) and hand it to
/// `build_geom`. `selected` (node ids classified in Pass B) become forced cut points.
pub(super) fn build_geometries<G, M>(
    index: &WayIndex<M>,
    coords: &NodeCoords,
    selected: &FxHashSet<i64>,
    build_geom: &G,
) -> anyhow::Result<()>
where
    G: Fn(&OsmWay, M) + Sync,
    M: Copy + Sync,
{
    use crate::profile::{self, RESOLVE};
    index.ways.par_iter().try_for_each(|&(id, start, len, m)| -> anyhow::Result<()> {
        let refs = &index.refs[start as usize..(start + len) as usize];
        if let Some(w) = profile::time(&RESOLVE, || resolve_geometry(id, refs, coords, selected)) {
            build_geom(&w, m);
        }
        Ok(())
    })
}
