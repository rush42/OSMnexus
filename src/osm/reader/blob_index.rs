//! Blob-index construction, sort detection, and the binary-search way-region boundary — everything
//! that inspects the PBF's blob layout without (fully) decoding way/node data.

use anyhow::{anyhow, Context};
use osmpbf::{BlobDecode, BlobReader, BlobType, ByteOffset, Mmap, PrimitiveBlock};

/// Build the blob index without decompressing any blob: record the byte offset of every
/// `OSMData` blob and the offset of the first `OSMHeader` blob.
pub(super) fn build_blob_index(path: &str) -> anyhow::Result<(Vec<ByteOffset>, Option<ByteOffset>)> {
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
pub(super) fn pbf_is_sorted(path: &str, header_off: ByteOffset) -> anyhow::Result<bool> {
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
pub(super) fn find_way_section_start(mmap: &Mmap, data: &[ByteOffset]) -> anyhow::Result<usize> {
    let (mut lo, mut hi) = (0usize, data.len());
    while lo < hi {
        let mid = (lo + hi) / 2;
        let k = blob_kind(&decode_block(mmap, data[mid])?);
        if k.has_ways || k.has_relations {
            hi = mid;
        } else {
            lo = mid + 1;
        }
    }
    let way_start = lo;

    // Sanity: the boundary must hold (guards a file that lies about its sort order).
    if way_start < data.len() {
        let k = blob_kind(&decode_block(mmap, data[way_start])?);
        if !(k.has_ways || k.has_relations) {
            return Err(anyhow!("boundary blob {way_start} has no ways/relations"));
        }
    }
    if way_start > 0 {
        let k = blob_kind(&decode_block(mmap, data[way_start - 1])?);
        if k.has_ways || k.has_relations || !k.has_nodes {
            return Err(anyhow!("blob {} before boundary is not node-only", way_start - 1));
        }
    }

    Ok(way_start)
}

/// Binary-search, within the way+relation tail `data[way_start..]`, the first blob that contains
/// relations. Valid because sorted files lay out way blobs before relation blobs. Returns an index
/// into the *full* `data` slice (so `data[rel_start..]` is the relation region). If no relations
/// exist, returns `data.len()`. Errors on a layout that interleaves ways after relations (the caller
/// falls back). `way_start` is the boundary from `find_way_section_start`.
pub(super) fn find_relation_section_start(
    mmap: &Mmap,
    data: &[ByteOffset],
    way_start: usize,
) -> anyhow::Result<usize> {
    let (mut lo, mut hi) = (way_start, data.len());
    while lo < hi {
        let mid = (lo + hi) / 2;
        let k = blob_kind(&decode_block(mmap, data[mid])?);
        if k.has_relations {
            hi = mid;
        } else {
            lo = mid + 1;
        }
    }
    let rel_start = lo;

    // Sanity: the boundary must hold — a relation region blob has relations, and the blob before it
    // (if inside the tail) must be way-only (no relations). Guards an interleaved/mislabeled file.
    if rel_start < data.len() {
        let k = blob_kind(&decode_block(mmap, data[rel_start])?);
        if !k.has_relations {
            return Err(anyhow!("boundary blob {rel_start} has no relations"));
        }
    }
    if rel_start > way_start {
        let k = blob_kind(&decode_block(mmap, data[rel_start - 1])?);
        if k.has_relations {
            return Err(anyhow!("blob {} before relation boundary still has relations", rel_start - 1));
        }
    }

    Ok(rel_start)
}

/// Which primitive kinds a block contains.
struct BlobKind {
    has_nodes: bool,
    has_ways: bool,
    has_relations: bool,
}

/// Classify a primitive block by the kinds of primitives it holds.
fn blob_kind(block: &PrimitiveBlock) -> BlobKind {
    let mut k = BlobKind { has_nodes: false, has_ways: false, has_relations: false };
    for g in block.groups() {
        if g.ways().next().is_some() {
            k.has_ways = true;
        }
        if g.relations().next().is_some() {
            k.has_relations = true;
        }
        if g.nodes().next().is_some() || g.dense_nodes().next().is_some() {
            k.has_nodes = true;
        }
        if k.has_nodes && k.has_ways && k.has_relations {
            break;
        }
    }
    k
}

/// Decode the `OSMData` primitive block at `off` directly from the shared memory map — no file
/// handle, no seek/read syscall per blob. `Mmap` is a read-only page-cache-backed view so this is
/// safe (and fast — no lock contention) to call concurrently from parallel rayon tasks.
pub(super) fn decode_block(mmap: &Mmap, off: ByteOffset) -> anyhow::Result<PrimitiveBlock> {
    let mut reader = mmap.blob_iter();
    reader.seek(off);
    let blob = reader
        .next()
        .ok_or_else(|| anyhow!("no blob at offset {off:?}"))?
        .with_context(|| format!("reading blob at {off:?}"))?;
    match blob.decode().with_context(|| format!("decoding blob at {off:?}"))? {
        BlobDecode::OsmData(block) => Ok(block),
        _ => Err(anyhow!("blob at {off:?} is not an OSMData blob")),
    }
}
