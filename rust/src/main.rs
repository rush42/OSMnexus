use std::sync::atomic::{AtomicUsize, Ordering};

use osm_pipeline::{config, db, engine, osm, processing};

use anyhow::Context;
use bytes::Bytes;
use clap::Parser;
use deadpool_postgres::Pool;
use futures::SinkExt;
use tokio::sync::mpsc;
use tracing::info;

use config::Config;
use db::{pool::build_pool, schema, schema::GEOM_TABLE};
use engine::runner::{GeomRow, TopicRow};
use engine::topic_runner::{stream_geom_rows, stream_rows, TopicRunner};
use osm::reader::stream_ways;
use osm::types::{ElementFilter, OsmWay, WayData};
use processing::{classify_way, geom_rows_for};

const TAG_COPY_COLUMNS: &str =
    "(osm_id, osm_type, id, osm, derived, private, meta, minzoom) FROM STDIN (FORMAT CSV)";
const GEOM_COPY_COLUMNS: &str =
    "(osm_id, variant, seg_idx, start_id, end_id, geom, length_m, total_length_m) FROM STDIN (FORMAT CSV)";

/// Per-writer channel capacity (rows/batches buffered before the producer blocks).
const WRITER_CHAN_CAP: usize = 256;

/// One COPY writer for the tag table `table`: owns its pooled connection for the whole COPY (so the
/// deadpool `Object` can't be recycled mid-COPY — the pitfall that hangs the next `copy_in`),
/// drains its channel, and returns the row count.
async fn tag_writer(
    pool: Pool,
    table: String,
    mut rx: mpsc::Receiver<Vec<TopicRow>>,
) -> anyhow::Result<usize> {
    let client = pool.get().await.context("getting tag writer connection")?;
    let mut sink = Box::pin(client.copy_in(&format!("COPY {table} {TAG_COPY_COLUMNS}")).await?);
    let mut buf = Vec::with_capacity(512 * 1024);
    let mut count = 0;
    while let Some(rows) = rx.recv().await {
        count += stream_rows(rows, &mut buf, sink.as_mut()).await?;
    }
    if !buf.is_empty() {
        sink.as_mut().send(Bytes::from(buf)).await?;
    }
    sink.as_mut().finish().await?;
    Ok(count)
}

/// One COPY writer for the shared `GEOM_TABLE`. Geometry is topic-independent (a way's line + its
/// global intersection split are the same for every topic), so it is written once per way here
/// rather than duplicated per topic.
async fn geom_writer(
    pool: Pool,
    mut rx: mpsc::Receiver<Vec<GeomRow>>,
) -> anyhow::Result<usize> {
    let client = pool.get().await.context("getting geom writer connection")?;
    let mut sink =
        Box::pin(client.copy_in(&format!("COPY {GEOM_TABLE} {GEOM_COPY_COLUMNS}")).await?);
    let mut buf = Vec::with_capacity(512 * 1024);
    let mut count = 0;
    while let Some(rows) = rx.recv().await {
        count += stream_geom_rows(&rows, &mut buf, sink.as_mut()).await?;
    }
    if !buf.is_empty() {
        sink.as_mut().send(Bytes::from(buf)).await?;
    }
    sink.as_mut().finish().await?;
    Ok(count)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt::init();

    let cfg = Config::parse();
    osm_pipeline::profile::init_from_env();

    // Size the rayon pool (CPU-bound decode/stream). `0` = rayon's default (logical CPU count).
    if cfg.threads > 0 {
        rayon::ThreadPoolBuilder::new()
            .num_threads(cfg.threads)
            .build_global()
            .context("configuring rayon thread pool")?;
        info!("rayon thread pool capped at {} threads", cfg.threads);
    }

    // Load all topics.  Add a new topic name here — no other code changes needed.
    let runners: Vec<TopicRunner> = ["bikelanes", "roads", "barrierLines"]
        .iter()
        .map(|name| TopicRunner::load(name))
        .collect::<anyhow::Result<_>>()?;

    let tables: Vec<String> = runners.iter().map(|r| r.table().to_owned()).collect();
    let table_refs: Vec<&str> = tables.iter().map(String::as_str).collect();

    for r in &runners {
        info!(
            "Loaded topic '{}' ({} categories, {} osm fields, {} sanitizers, {} derivers)",
            r.table(),
            r.categories.categories.len(),
            r.spec.osm_fields.len(),
            r.sanitizer_fields.len(),
            r.topic_derivers.len()
        );
    }

    info!("Connecting to database {}@{}/{}", cfg.db_user, cfg.db_host, cfg.db_name);
    let pool = build_pool(&cfg)?;
    let client_setup = pool.get().await.context("getting DB connection")?;

    info!("Setting up schema...");
    schema::create_tables(&client_setup, &table_refs).await?;
    if cfg.truncate {
        schema::truncate_tables(&client_setup, &table_refs).await?;
    }
    schema::drop_indexes(&client_setup, &table_refs).await?;
    drop(client_setup);

    // Each topic declares which ways it needs (data-defined `element_filter` in topic.json,
    // defaulting to `highway`). The reader keeps any way matching any topic's filter.
    let filters: Vec<ElementFilter> = runners
        .iter()
        .map(|r| r.spec.element_filter.clone().unwrap_or_else(ElementFilter::highway))
        .collect();

    let n = tables.len();
    let k = cfg.db_writers.max(1);
    // Pool must supply every writer connection at once: k per tag table + k for the shared geom table.
    pool.resize((n + 1) * k + 2);
    info!("Using {k} COPY connection(s) per table ({} writers total)", (n + 1) * k);

    info!("Reading + processing PBF (streaming): {}", cfg.pbf_file);
    let t0 = std::time::Instant::now();

    // Sharded COPY writers: k connections per tag table, plus k connections for the one shared geom
    // table. Rows are round-robined across a table's k writers, so the dominant tables get k-way
    // parallelism for both serialization and ingest instead of one serial connection.
    let mut tag_senders: Vec<Vec<mpsc::Sender<Vec<TopicRow>>>> = Vec::with_capacity(n);
    let mut tag_handles: Vec<Vec<tokio::task::JoinHandle<anyhow::Result<usize>>>> = Vec::with_capacity(n);
    for table in &tables {
        let (mut ts, mut th) = (Vec::with_capacity(k), Vec::with_capacity(k));
        for _ in 0..k {
            let (tx, rx) = mpsc::channel::<Vec<TopicRow>>(WRITER_CHAN_CAP);
            th.push(tokio::spawn(tag_writer(pool.clone(), table.clone(), rx)));
            ts.push(tx);
        }
        tag_senders.push(ts);
        tag_handles.push(th);
    }
    let mut geom_senders: Vec<mpsc::Sender<Vec<GeomRow>>> = Vec::with_capacity(k);
    let mut geom_handles: Vec<tokio::task::JoinHandle<anyhow::Result<usize>>> = Vec::with_capacity(k);
    for _ in 0..k {
        let (tx, rx) = mpsc::channel::<Vec<GeomRow>>(WRITER_CHAN_CAP);
        geom_handles.push(tokio::spawn(geom_writer(pool.clone(), rx)));
        geom_senders.push(tx);
    }

    // Producer: the reader decodes the PBF once and drives two callbacks. `classify` (Pass A) turns a
    // way's tags into per-topic tag rows, routed round-robin to each topic's tag writers; it returns
    // `Some(())` iff some topic kept the way. `build_geom` (geometry pass) writes the resolved way's
    // geometry **once** to the shared geom table (topic-independent). Runs on the blocking pool
    // (CPU-bound rayon work). Dropping the producer drops all senders, closing the writer channels.
    let pbf_file = cfg.pbf_file.clone();
    let split = cfg.split;
    let tag_rr: Vec<AtomicUsize> = (0..n).map(|_| AtomicUsize::new(0)).collect();
    let geom_rr = AtomicUsize::new(0);
    let producer = tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        let classify = |wd: &WayData| -> Option<()> {
            let out = classify_way(&runners, wd);
            if out.mask == 0 {
                return None;
            }
            for (i, rows) in out.topic_rows.into_iter().enumerate() {
                if !rows.is_empty() {
                    let kk = tag_rr[i].fetch_add(1, Ordering::Relaxed) % k;
                    let _ = tag_senders[i][kk].blocking_send(rows);
                }
            }
            Some(())
        };
        let build_geom = |way: &OsmWay, _kept: ()| {
            let kk = geom_rr.fetch_add(1, Ordering::Relaxed) % k;
            let _ = geom_senders[kk].blocking_send(geom_rows_for(way, split));
        };
        stream_ways(&pbf_file, &filters, classify, build_geom)
    });

    // Await the producer first: it drops the senders, closing every writer channel so the writer
    // tasks drain their tails, finish the COPY, and return their counts.
    producer.await.context("reader/processing task panicked")??;
    osm_pipeline::profile::report();

    let mut tag_counts = vec![0usize; n];
    for (i, handles) in tag_handles.into_iter().enumerate() {
        for h in handles {
            tag_counts[i] += h.await.context("tag writer panicked")??;
        }
    }
    let mut geom_count = 0usize;
    for h in geom_handles {
        geom_count += h.await.context("geom writer panicked")??;
    }

    for (i, table) in tables.iter().enumerate() {
        info!("Wrote {} tag rows → {}", tag_counts[i], table);
    }
    info!("Wrote {geom_count} geom rows → {GEOM_TABLE}");
    info!("Read + process time: {:.1}s", t0.elapsed().as_secs_f32());

    if cfg.create_index {
        info!("Creating indexes...");
        let t_idx = std::time::Instant::now();
        schema::create_indexes(&pool, &table_refs).await?;
        info!("Index creation: {:.1}s", t_idx.elapsed().as_secs_f32());
    } else {
        info!("Skipping index creation (pass --create-index to enable)");
    }

    info!("Done. Total: {:.1}s", t0.elapsed().as_secs_f32());
    Ok(())
}
