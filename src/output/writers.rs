//! Output backends: one buffered writer per (table, shard). Both backends drain a channel of row
//! batches and serialize each row with the same `CsvRow::csv_fields`, so the two paths differ only
//! in their sink (a Postgres COPY stream vs a buffered file).

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::PathBuf;

use anyhow::Context;
use bytes::Bytes;
use deadpool_postgres::Pool;
use futures::SinkExt;
use tokio::sync::mpsc;

use crate::output::rows::{write_csv_row, CsvRow};

/// Flush the byte buffer to the sink once it reaches this size.
const FLUSH_BYTES: usize = 512 * 1024;

/// One COPY writer for `table`: owns its pooled connection for the whole COPY (so the deadpool
/// `Object` can't be recycled mid-COPY — the pitfall that hangs the next `copy_in`), drains its
/// channel into a `COPY {table} ({columns}) FROM STDIN (FORMAT CSV)` sink, and returns the row
/// count. Generic over the row type: tag tables take `TopicRow`, the shared geom table `EdgeRow`.
pub async fn copy_writer<R: CsvRow>(
    pool: Pool,
    table: String,
    columns: &'static str,
    mut rx: mpsc::Receiver<Vec<R>>,
) -> anyhow::Result<usize> {
    let client = pool.get().await.context("getting COPY writer connection")?;
    let mut sink = Box::pin(
        client.copy_in(&format!("COPY {table} ({columns}) FROM STDIN (FORMAT CSV)")).await?,
    );
    let mut buf = Vec::with_capacity(FLUSH_BYTES);
    let mut count = 0;
    while let Some(rows) = rx.recv().await {
        for row in &rows {
            write_csv_row(&mut buf, &row.csv_fields()?);
            count += 1;
            if buf.len() >= FLUSH_BYTES {
                sink.as_mut().send(Bytes::from(std::mem::take(&mut buf))).await?;
                buf = Vec::with_capacity(FLUSH_BYTES);
            }
        }
    }
    if !buf.is_empty() {
        sink.as_mut().send(Bytes::from(buf)).await?;
    }
    sink.as_mut().finish().await?;
    Ok(count)
}

/// One CSV file writer: writes the `header` line, then each row's CSV record to a buffered file,
/// reusing the same field serialization as the COPY path. Returns the row count.
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
    let mut count = 0;
    while let Some(rows) = rx.recv().await {
        for row in &rows {
            write_csv_row(&mut buf, &row.csv_fields()?);
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
