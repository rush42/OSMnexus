//! Blob-index construction, sort detection, and the binary-search way-region boundary — everything
//! that inspects the PBF's blob layout without (fully) decoding way/node data.

use anyhow::{anyhow, Context};
use osmpbf::{BlobReader, BlobType, ByteOffset, PrimitiveBlock};

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
pub(super) fn find_way_section_start(path: &str, data: &[ByteOffset]) -> anyhow::Result<usize> {
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
pub(super) fn decode_block(path: &str, off: ByteOffset) -> anyhow::Result<PrimitiveBlock> {
    let mut reader = BlobReader::seekable_from_path(path).context("opening PBF for blob")?;
    let blob = reader.blob_from_offset(off).with_context(|| format!("reading blob at {off:?}"))?;
    blob.to_primitiveblock().with_context(|| format!("decoding blob at {off:?}"))
}
