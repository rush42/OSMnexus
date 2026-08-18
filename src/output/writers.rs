//! Output sinks: one buffered writer per (table, shard). Every sink starts from the same input —
//! the pipeline's **row structs** (`TopicRow`/`MemberRow`/`EdgeRow`/`GeomRow`/`NodeRow`) arriving in
//! batches over a channel — and each owns its encoding of them (see `output::rows`' own doc):
//! `copy_writer` encodes to Postgres `COPY ... (FORMAT BINARY)` and streams it into a live
//! connection (`--output pg`); `csv_writer` writes CSV text (`--output csv`); `stage_writer` writes
//! the run's own staging format (`--output geojson`/`geojson-seq`/`parquet`, read back afterwards by
//! `output::cursor`'s join — see `output::stage`); `memory_sink` skips encoding entirely and hands
//! the typed rows straight to an in-process consumer, for embedding the pipeline as a library with
//! no serialization or disk round trip at all.
//!
//! `stage_writer` used to be `binary_file_writer`, dumping Postgres's wire bytes to a file so the
//! file backends could re-read them through a Postgres decoder. `output::stage`'s doc records why
//! that is gone.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::PathBuf;

use anyhow::Context;
use bytes::Bytes;
use deadpool_postgres::Pool;
use futures::SinkExt;
use tokio::sync::mpsc;

use crate::output::rows::{
    write_binary_header, write_binary_row, write_binary_trailer, write_csv_row, BinaryRow, CsvRow,
};
use crate::output::stage::{StageRow, StageWriter};

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

/// One staging-file writer: encodes each row with `StageRow` into `path`, framed by
/// `output::stage`'s `StageWriter`, for `output::cursor`'s post-run join to read back. No table
/// name or `columns` list needed — a staged row is self-describing, so unlike the `pg`/`csv` sinks
/// there is no column set to agree on.
pub async fn stage_writer<R: StageRow>(
    path: PathBuf,
    mut rx: mpsc::Receiver<Vec<R>>,
) -> anyhow::Result<usize> {
    let file = File::create(&path).with_context(|| format!("creating {}", path.display()))?;
    let mut writer = StageWriter::new(BufWriter::new(file))?;
    while let Some(rows) = rx.recv().await {
        for row in rows {
            writer.write_row(&row)?;
        }
    }
    Ok(writer.finish()?)
}

/// One CSV file writer: writes the `header` line, then each row's CSV record (`CsvRow`) to a
/// buffered file. Returns the row count. `fields` is hoisted out of the loop and cleared per row so
/// the whole table costs one `Vec<String>`, not one per row.
pub async fn csv_writer<R: CsvRow>(
    path: PathBuf,
    header: &'static str,
    mut rx: mpsc::Receiver<Vec<R>>,
) -> anyhow::Result<usize> {
    let mut f = BufWriter::new(
        File::create(&path).with_context(|| format!("creating {}", path.display()))?,
    );
    writeln!(f, "{header}")?;
    let mut buf = Vec::with_capacity(FLUSH_BYTES);
    let mut fields: Vec<String> = Vec::new();
    let mut count = 0;
    while let Some(rows) = rx.recv().await {
        for row in rows {
            fields.clear();
            row.csv_fields(&mut fields);
            write_csv_row(&mut buf, &fields);
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
