//! The pipeline's own staging format for `{table}.bin` — the on-disk shape of the *struct* contract
//! every output row type speaks (`StageRow`/`StageDecode`).
//!
//! Staging used to reuse Postgres's `COPY ... (FORMAT BINARY)` wire bytes, so `--output geojson`/
//! `geojsonseq`/`parquet` re-read their own output through a Postgres decoder. That made the
//! Postgres wire format a *contract* between the pipeline and every writer, and it cost:
//!
//!   * two `Vec` allocations per row on the round trip (`binary_fields()` built one, `read_binary_row`
//!     another), tens of millions of times on a country-sized run;
//!   * an out-of-band schema (`BinaryFieldType`) at every read site, because COPY tuples carry a
//!     length prefix per field but no type tag — and a `peek_tag_field_count` hack to recover
//!     whether the tag table even has an `id` column;
//!   * a per-row `String` for `category`, interned to one `Arc<str>` on the way out and flattened
//!     back to an owned copy on the way in, since the wire format can't express sharing.
//!
//! Postgres's encoding now lives only in `copy_writer`, where it belongs. This format is ours:
//!
//! ```text
//! magic    "OSMNXST1"                       (8 bytes)
//! frame    u32 row_count | u32 payload_len | payload   (repeated)
//! trailer  u32 0         | u32 0
//! ```
//!
//! Rows are self-describing (an absent `id` is an explicit `None`, not a missing column), so no
//! caller-supplied schema and no field-count peeking. Frames matter for more than buffering: a
//! reader can reach any frame by walking 8-byte frame headers — O(frames), not O(bytes) — so the
//! file can be split across workers without decoding it first, which the Postgres format could not
//! support at all (a tuple's extent is only knowable by walking its own field length prefixes).
//! That is what the planned parallel column-encode work needs, and the reason to change the format
//! before building on top of it rather than after.
//!
//! Values are little-endian and fixed-width where the type is (`i64`/`f64`/discriminants), so a
//! frame decodes out of an in-memory slice with no per-field `read_exact`.

use std::fs::File;
use std::io::{BufReader, Read, Write};
use std::path::Path;

use anyhow::Context;

/// File signature. Bump the trailing digit on any incompatible layout change — staging files never
/// outlive the run that wrote them, so there is no migration to do, only a mismatch to report.
const MAGIC: &[u8; 8] = b"OSMNXST1";

/// Start a new frame once the pending payload reaches this size. Same 512 KiB the sinks already
/// flushed at, so frame count tracks what write buffering was doing anyway; it also caps a reader's
/// working set, since `StageReader` decodes a frame from one in-memory buffer.
const FRAME_BYTES: usize = 512 * 1024;

/// A `None` marker for length-prefixed optional fields — no real string or blob is 4 GiB.
const NONE_LEN: u32 = u32::MAX;

// --- encoding primitives ----------------------------------------------------

pub fn put_u8(buf: &mut Vec<u8>, v: u8) {
    buf.push(v);
}

pub fn put_u32(buf: &mut Vec<u8>, v: u32) {
    buf.extend_from_slice(&v.to_le_bytes());
}

pub fn put_i64(buf: &mut Vec<u8>, v: i64) {
    buf.extend_from_slice(&v.to_le_bytes());
}

pub fn put_f64(buf: &mut Vec<u8>, v: f64) {
    buf.extend_from_slice(&v.to_le_bytes());
}

/// `None` encodes as the `NONE_LEN` sentinel, which is what makes an omitted `id`/`category` a
/// value in the row rather than a missing column the reader has to infer.
pub fn put_opt_f64(buf: &mut Vec<u8>, v: Option<f64>) {
    match v {
        Some(v) => {
            put_u8(buf, 1);
            put_f64(buf, v);
        }
        None => put_u8(buf, 0),
    }
}

pub fn put_bytes(buf: &mut Vec<u8>, v: &[u8]) {
    put_u32(buf, v.len() as u32);
    buf.extend_from_slice(v);
}

pub fn put_str(buf: &mut Vec<u8>, v: &str) {
    put_bytes(buf, v.as_bytes());
}

pub fn put_opt_str(buf: &mut Vec<u8>, v: Option<&str>) {
    match v {
        Some(s) => put_str(buf, s),
        None => put_u32(buf, NONE_LEN),
    }
}

// --- decoding ---------------------------------------------------------------

/// A position in one decoded frame's payload. Every read is a bounds-checked slice out of memory —
/// no `io::Read` per field, and no allocation except where the row type genuinely owns a value.
pub struct StageCursor<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> StageCursor<'a> {
    /// A cursor over one frame's payload — or, for a caller decoding a single row it encoded
    /// itself (`StageRow`/`StageDecode` round-trip tests), over just those bytes.
    pub fn new(buf: &'a [u8]) -> Self {
        StageCursor { buf, pos: 0 }
    }

    fn take(&mut self, n: usize) -> anyhow::Result<&'a [u8]> {
        let end = self.pos.checked_add(n).context("stage row length overflow")?;
        anyhow::ensure!(end <= self.buf.len(), "truncated stage frame: want {n} bytes at {}", self.pos);
        let out = &self.buf[self.pos..end];
        self.pos = end;
        Ok(out)
    }

    pub fn u8(&mut self) -> anyhow::Result<u8> {
        Ok(self.take(1)?[0])
    }

    pub fn u32(&mut self) -> anyhow::Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into()?))
    }

    pub fn i64(&mut self) -> anyhow::Result<i64> {
        Ok(i64::from_le_bytes(self.take(8)?.try_into()?))
    }

    pub fn f64(&mut self) -> anyhow::Result<f64> {
        Ok(f64::from_le_bytes(self.take(8)?.try_into()?))
    }

    pub fn opt_f64(&mut self) -> anyhow::Result<Option<f64>> {
        match self.u8()? {
            0 => Ok(None),
            _ => Ok(Some(self.f64()?)),
        }
    }

    /// Borrowed from the frame buffer — a decoder that only needs to *look* at a string (matching
    /// `osm_type`/`geom_type` onto a `&'static str`, or interning a category) never allocates.
    pub fn str(&mut self) -> anyhow::Result<&'a str> {
        let len = self.u32()?;
        anyhow::ensure!(len != NONE_LEN, "unexpected null in non-optional string field");
        Ok(std::str::from_utf8(self.take(len as usize)?)?)
    }

    pub fn opt_str(&mut self) -> anyhow::Result<Option<&'a str>> {
        let len = self.u32()?;
        if len == NONE_LEN {
            return Ok(None);
        }
        Ok(Some(std::str::from_utf8(self.take(len as usize)?)?))
    }

    pub fn bytes(&mut self) -> anyhow::Result<&'a [u8]> {
        let len = self.u32()?;
        anyhow::ensure!(len != NONE_LEN, "unexpected null in non-optional bytes field");
        self.take(len as usize)
    }
}

// --- the contract -----------------------------------------------------------

/// A row that can be staged to disk. Takes `&self` rather than consuming: encoding copies bytes out
/// of the row's fields either way, so there is nothing to gain by moving them into an intermediate.
pub trait StageRow {
    fn stage_encode(&self, buf: &mut Vec<u8>);
}

/// The reverse of [`StageRow`]. `Ctx` is per-reader decode state that outlives a single row —
/// `TopicRow` uses it to intern `category` (one `Arc<str>` per distinct value for the whole file,
/// instead of one owned `String` per row); row types that need none set it to `()`.
pub trait StageDecode: Sized {
    type Ctx: Default;
    fn stage_decode(cur: &mut StageCursor<'_>, ctx: &mut Self::Ctx) -> anyhow::Result<Self>;
}

/// Reader-side string interning — see [`StageDecode::Ctx`]. Small by construction (a topic's
/// distinct category ids), so the map costs nothing next to the per-row allocations it removes.
#[derive(Default)]
pub struct Interner {
    seen: std::collections::HashMap<Box<str>, std::sync::Arc<str>>,
}

impl Interner {
    pub fn intern(&mut self, s: &str) -> std::sync::Arc<str> {
        if let Some(existing) = self.seen.get(s) {
            return existing.clone();
        }
        let shared: std::sync::Arc<str> = std::sync::Arc::from(s);
        self.seen.insert(Box::from(s), shared.clone());
        shared
    }
}

// --- writer -----------------------------------------------------------------

/// Frames rows into `w` (see the module doc's layout). Rows accumulate in one reusable buffer and
/// cross to the writer a whole frame at a time.
pub struct StageWriter<W: Write> {
    inner: W,
    payload: Vec<u8>,
    rows_in_frame: u32,
    total: usize,
}

impl<W: Write> StageWriter<W> {
    pub fn new(mut inner: W) -> std::io::Result<Self> {
        inner.write_all(MAGIC)?;
        Ok(StageWriter { inner, payload: Vec::with_capacity(FRAME_BYTES + 8192), rows_in_frame: 0, total: 0 })
    }

    pub fn write_row<R: StageRow>(&mut self, row: &R) -> std::io::Result<()> {
        row.stage_encode(&mut self.payload);
        self.rows_in_frame += 1;
        self.total += 1;
        if self.payload.len() >= FRAME_BYTES {
            self.flush_frame()?;
        }
        Ok(())
    }

    fn flush_frame(&mut self) -> std::io::Result<()> {
        if self.rows_in_frame == 0 {
            return Ok(());
        }
        self.inner.write_all(&self.rows_in_frame.to_le_bytes())?;
        self.inner.write_all(&(self.payload.len() as u32).to_le_bytes())?;
        self.inner.write_all(&self.payload)?;
        self.payload.clear();
        self.rows_in_frame = 0;
        Ok(())
    }

    /// Flush the open frame, write the zero-length trailer, and return the row count.
    pub fn finish(mut self) -> std::io::Result<usize> {
        self.flush_frame()?;
        self.inner.write_all(&0u32.to_le_bytes())?;
        self.inner.write_all(&0u32.to_le_bytes())?;
        self.inner.flush()?;
        Ok(self.total)
    }
}

// --- reader -----------------------------------------------------------------

/// Streams `R`s back out of a staging file, one frame in memory at a time — so a multi-gigabyte
/// `{table}.bin` costs `FRAME_BYTES` of buffer regardless of its size.
pub struct StageReader<R: StageDecode> {
    inner: BufReader<File>,
    frame: Vec<u8>,
    pos: usize,
    remaining: u32,
    ctx: R::Ctx,
    done: bool,
}

impl<R: StageDecode> StageReader<R> {
    /// `Ok(None)` if `path` doesn't exist — for the tables a run legitimately never wrote (a topic
    /// that declared no geometry of some kind, or no relation members at all).
    pub fn open_optional(path: &Path) -> anyhow::Result<Option<Self>> {
        if !path.exists() {
            return Ok(None);
        }
        Ok(Some(Self::open(path)?))
    }

    pub fn open(path: &Path) -> anyhow::Result<Self> {
        let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
        let mut inner = BufReader::new(file);
        let mut magic = [0u8; 8];
        inner
            .read_exact(&mut magic)
            .with_context(|| format!("reading stage header of {}", path.display()))?;
        anyhow::ensure!(&magic == MAGIC, "{} is not a staging file", path.display());
        Ok(StageReader {
            inner,
            frame: Vec::new(),
            pos: 0,
            remaining: 0,
            ctx: R::Ctx::default(),
            done: false,
        })
    }

    /// Read the next frame's payload into `self.frame`. `false` at the trailer.
    fn next_frame(&mut self) -> anyhow::Result<bool> {
        let mut header = [0u8; 8];
        match self.inner.read_exact(&mut header) {
            Ok(()) => {}
            // A staging file always ends in an explicit trailer; tolerate its absence rather than
            // erroring, so a truncated file reports what it did contain.
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(false),
            Err(e) => return Err(e).context("reading stage frame header"),
        }
        let rows = u32::from_le_bytes(header[0..4].try_into()?);
        let len = u32::from_le_bytes(header[4..8].try_into()?) as usize;
        if rows == 0 {
            return Ok(false);
        }
        self.frame.resize(len, 0);
        self.inner.read_exact(&mut self.frame).context("reading stage frame payload")?;
        self.pos = 0;
        self.remaining = rows;
        Ok(true)
    }

    /// The next row, or `None` at end of data.
    pub fn next_row(&mut self) -> anyhow::Result<Option<R>> {
        while self.remaining == 0 {
            if self.done || !self.next_frame()? {
                self.done = true;
                return Ok(None);
            }
        }
        let mut cur = StageCursor { buf: &self.frame, pos: self.pos };
        let row = R::stage_decode(&mut cur, &mut self.ctx)?;
        self.pos = cur.pos;
        self.remaining -= 1;
        Ok(Some(row))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Probe {
        id: i64,
        name: Option<String>,
        blob: Vec<u8>,
        len: Option<f64>,
    }

    impl StageRow for Probe {
        fn stage_encode(&self, buf: &mut Vec<u8>) {
            put_i64(buf, self.id);
            put_opt_str(buf, self.name.as_deref());
            put_bytes(buf, &self.blob);
            put_opt_f64(buf, self.len);
        }
    }

    impl StageDecode for Probe {
        type Ctx = ();
        fn stage_decode(cur: &mut StageCursor<'_>, _: &mut ()) -> anyhow::Result<Self> {
            Ok(Probe {
                id: cur.i64()?,
                name: cur.opt_str()?.map(str::to_owned),
                blob: cur.bytes()?.to_vec(),
                len: cur.opt_f64()?,
            })
        }
    }

    fn round_trip(rows: Vec<Probe>) -> Vec<Probe> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("probe.bin");
        let mut w = StageWriter::new(std::io::BufWriter::new(File::create(&path).unwrap())).unwrap();
        for row in &rows {
            w.write_row(row).unwrap();
        }
        assert_eq!(w.finish().unwrap(), rows.len());

        let mut r = StageReader::<Probe>::open(&path).unwrap();
        let mut out = Vec::new();
        while let Some(row) = r.next_row().unwrap() {
            out.push(row);
        }
        out
    }

    #[test]
    fn round_trips_present_and_absent_optionals() {
        let out = round_trip(vec![
            Probe { id: 1, name: Some("a".to_owned()), blob: vec![1, 2, 3], len: Some(4.5) },
            Probe { id: -2, name: None, blob: Vec::new(), len: None },
        ]);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].id, 1);
        assert_eq!(out[0].name.as_deref(), Some("a"));
        assert_eq!(out[0].blob, vec![1, 2, 3]);
        assert_eq!(out[0].len, Some(4.5));
        assert_eq!(out[1].id, -2);
        assert_eq!(out[1].name, None);
        assert!(out[1].blob.is_empty());
        assert_eq!(out[1].len, None);
    }

    #[test]
    fn round_trips_across_frame_boundaries() {
        // Each row carries a 64 KiB blob, so this spans several `FRAME_BYTES` frames and exercises
        // the reader's frame-refill path rather than staying inside one buffer.
        let rows: Vec<Probe> = (0..40)
            .map(|i| Probe { id: i, name: Some(format!("row{i}")), blob: vec![i as u8; 64 * 1024], len: None })
            .collect();
        let out = round_trip(rows);
        assert_eq!(out.len(), 40);
        for (i, row) in out.iter().enumerate() {
            assert_eq!(row.id, i as i64);
            assert_eq!(row.name.as_deref(), Some(format!("row{i}").as_str()));
            assert_eq!(row.blob.len(), 64 * 1024);
            assert_eq!(row.blob[0], i as u8);
        }
    }

    #[test]
    fn empty_file_reads_back_as_no_rows() {
        assert!(round_trip(Vec::new()).is_empty());
    }

    #[test]
    fn interner_returns_one_shared_handle_per_distinct_value() {
        let mut interner = Interner::default();
        let a1 = interner.intern("road");
        let a2 = interner.intern("road");
        let b = interner.intern("path");
        assert!(std::sync::Arc::ptr_eq(&a1, &a2));
        assert!(!std::sync::Arc::ptr_eq(&a1, &b));
        assert_eq!(&*b, "path");
    }

    #[test]
    fn rejects_a_file_that_is_not_a_staging_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bogus.bin");
        std::fs::write(&path, b"PGCOPY\n\xff\r\n\0nope").unwrap();
        assert!(StageReader::<Probe>::open(&path).is_err());
    }
}
