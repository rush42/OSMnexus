//! Output sinks: one buffered writer per (table, shard). All start from the same
//! `BinaryRow::binary_fields` encoding (see `output::rows`' own doc for why there's only one), then
//! diverge on *where* the encoded row goes: `copy_writer` streams it straight into a live Postgres
//! `COPY ... (FORMAT BINARY)` connection (`--output pg`); `binary_file_writer` writes the same wire
//! bytes to a plain file instead (`--output geojson`/`geojson-seq` staging, read back later by
//! `output::geojson`'s cursor join); `csv_writer` stringifies the fields
//! (`binary_fields_to_csv_row`) and writes text (`--output csv`, plus `geojson`/`geojson-seq`'s own
//! `.csv`-shaped deliverables where they still apply); `memory_sink` skips encoding entirely and
//! hands the typed rows straight to an in-process consumer — for embedding the pipeline as a
//! library with no disk round trip at all.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::PathBuf;

use anyhow::Context;
use bytes::Bytes;
use deadpool_postgres::Pool;
use futures::SinkExt;
use tokio::sync::mpsc;

use crate::output::rows::{
    binary_fields_to_csv_row, write_binary_header, write_binary_row, write_binary_trailer,
    write_csv_row, BinaryRow,
};

/// Flush the byte buffer to the sink once it reaches this size.
const FLUSH_BYTES: usize = 512 * 1024;

/// One COPY writer for `table`: owns its pooled connection for the whole COPY (so the deadpool
/// `Object` can't be recycled mid-COPY — the pitfall that hangs the next `copy_in`), drains its
/// channel into a `COPY {table} ({columns}) FROM STDIN (FORMAT BINARY)` sink, and returns the row
/// count. Generic over the row type: tag tables take `TopicRow`, the shared geom table `EdgeRow`.
/// Binary (not CSV/text) so both the client and Postgres skip text-formatting/re-parsing every
/// value (floats, hex-encoded geometry) — see `output::rows::BinaryField` for the wire encoding.
pub async fn copy_writer<R: BinaryRow>(
    pool: Pool,
    table: String,
    columns: &'static str,
    mut rx: mpsc::Receiver<Vec<R>>,
) -> anyhow::Result<usize> {
    let client = pool.get().await.context("getting COPY writer connection")?;
    let mut sink = Box::pin(
        client.copy_in(&format!("COPY {table} ({columns}) FROM STDIN (FORMAT BINARY)")).await?,
    );
    let mut buf = Vec::with_capacity(FLUSH_BYTES);
    write_binary_header(&mut buf);
    let mut count = 0;
    while let Some(rows) = rx.recv().await {
        for row in rows {
            write_binary_row(&mut buf, &row.binary_fields()?);
            count += 1;
            if buf.len() >= FLUSH_BYTES {
                sink.as_mut().send(Bytes::from(std::mem::take(&mut buf))).await?;
                buf = Vec::with_capacity(FLUSH_BYTES);
            }
        }
    }
    write_binary_trailer(&mut buf);
    sink.as_mut().send(Bytes::from(buf)).await?;
    sink.as_mut().finish().await?;
    Ok(count)
}

/// One binary-format file writer: the same wire bytes `copy_writer` streams to Postgres, written to
/// a plain file instead — used to stage `geojson`/`geojson-seq`'s tag/geometry/edges/relation-member
/// tables, later decoded back by `output::geojson`'s cursor join (`read_binary_row`/
/// `FromBinaryRow`). No `columns`/table name needed — no SQL involved, just the header/rows/trailer
/// framing `write_binary_header`/`write_binary_row`/`write_binary_trailer` already define.
pub async fn binary_file_writer<R: BinaryRow>(
    path: PathBuf,
    mut rx: mpsc::Receiver<Vec<R>>,
) -> anyhow::Result<usize> {
    let mut f = BufWriter::new(
        File::create(&path).with_context(|| format!("creating {}", path.display()))?,
    );
    let mut buf = Vec::with_capacity(FLUSH_BYTES);
    write_binary_header(&mut buf);
    let mut count = 0;
    while let Some(rows) = rx.recv().await {
        for row in rows {
            write_binary_row(&mut buf, &row.binary_fields()?);
            count += 1;
            if buf.len() >= FLUSH_BYTES {
                f.write_all(&buf)?;
                buf.clear();
            }
        }
    }
    write_binary_trailer(&mut buf);
    f.write_all(&buf)?;
    f.flush()?;
    Ok(count)
}

/// One CSV file writer: writes the `header` line, then each row's CSV record (via the shared
/// `binary_fields()` → `binary_fields_to_csv_row` path — see this module's own doc) to a buffered
/// file. Returns the row count.
pub async fn csv_writer<R: BinaryRow>(
    path: PathBuf,
    header: &'static str,
    mut rx: mpsc::Receiver<Vec<R>>,
) -> anyhow::Result<usize> {
    let mut f = BufWriter::new(
        File::create(&path).with_context(|| format!("creating {}", path.display()))?,
    );
    writeln!(f, "{header}")?;
    let mut buf = Vec::with_capacity(FLUSH_BYTES);
    let mut count = 0;
    while let Some(rows) = rx.recv().await {
        for row in rows {
            write_csv_row(&mut buf, &binary_fields_to_csv_row(row.binary_fields()?));
            count += 1;
        }
        if buf.len() >= FLUSH_BYTES {
            f.write_all(&buf)?;
            buf.clear();
        }
    }
    f.write_all(&buf)?;
    f.flush()?;
    Ok(count)
}

/// No-encoding sink: forwards row batches straight to an in-process consumer instead of a file or
/// Postgres connection — for embedding the pipeline as a library (e.g. a future live-editor
/// integration) with no serialization or disk round trip at all. Returns the row count, same as
/// every other writer task, once `tx`'s receiver end is dropped and `rx` drains empty.
pub async fn memory_sink<R: Send + 'static>(
    tx: mpsc::UnboundedSender<Vec<R>>,
    mut rx: mpsc::Receiver<Vec<R>>,
) -> anyhow::Result<usize> {
    let mut count = 0;
    while let Some(rows) = rx.recv().await {
        count += rows.len();
        // The receiver may already be gone (caller stopped listening) — not an error for this
        // writer task, just nothing left to forward; keep draining `rx` so upstream `send`s don't
        // block.
        let _ = tx.send(rows);
    }
    Ok(count)
}
