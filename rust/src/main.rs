use osm_pipeline::{config, db, engine, osm, processing};

use anyhow::Context;
use bytes::Bytes;
use clap::Parser;
use futures::SinkExt;
use tracing::info;

use config::Config;
use db::{pool::build_pool, schema};
use engine::runner::{GeomRow, TopicRow};
use engine::topic_runner::{stream_geom_rows, stream_rows, TopicRunner};
use osm::reader::stream_ways;
use osm::types::{ElementFilter, OsmWay, WayData};
use processing::{classify_way, geom_rows_for};

/// A batch of rows for the COPY consumer, tagged with its destination. `Tag(i, ..)` → tag sink of
/// topic `i`; `Geom(mask, ..)` → geom sink of every topic whose bit is set in `mask` (geometry is
/// shared across topics, so one build is fanned out to each surviving topic).
enum RowBatch {
    Tag(usize, Vec<TopicRow>),
    Geom(u32, Vec<GeomRow>),
}

const TAG_COPY_COLUMNS: &str =
    "(osm_id, osm_type, id, osm, derived, private, meta, minzoom) FROM STDIN (FORMAT CSV)";
const GEOM_COPY_COLUMNS: &str =
    "(osm_id, variant, seg_idx, start_id, end_id, geom, length_m, total_length_m) FROM STDIN (FORMAT CSV)";

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

    info!("Reading + processing PBF (streaming): {}", cfg.pbf_file);
    let t0 = std::time::Instant::now();

    let (tx, mut rx) = tokio::sync::mpsc::channel::<RowBatch>(2048);

    // Producer: the reader decodes the PBF once and drives two callbacks, both feeding the COPY
    // consumer below. `classify` (Pass A) turns a way's tags into per-topic tag rows and returns a
    // bitmask of which topics kept it; `build_geom` (geometry pass) turns the resolved way into geom
    // rows. Runs on the blocking pool because the reader is CPU-bound rayon work. Peak memory tracks
    // the node coordinates + the kept ways' node-id index, not the tag/geom row count.
    let pbf_file = cfg.pbf_file.clone();
    let split = cfg.split;
    let producer = tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        let classify = |wd: &WayData| -> Option<u32> {
            let out = classify_way(&runners, wd);
            if out.mask == 0 {
                return None;
            }
            for (i, rows) in out.topic_rows.into_iter().enumerate() {
                if !rows.is_empty() {
                    let _ = tx.blocking_send(RowBatch::Tag(i, rows));
                }
            }
            Some(out.mask)
        };
        let build_geom = |way: &OsmWay, mask: u32| {
            let _ = tx.blocking_send(RowBatch::Geom(mask, geom_rows_for(way, split)));
        };
        stream_ways(&pbf_file, &filters, classify, build_geom)
    });

    // One COPY sink per topic, pinned in a Box so they can live in a Vec.
    //
    // The `CopyInSink` returned by `copy_in` does NOT keep its deadpool `Object`
    // alive — it only holds a handle to the connection's driver. We must therefore
    // retain each `Object` for as long as its sink is in use; otherwise the `Object`
    // drops at the end of the loop iteration and the connection is recycled back into
    // the pool *while still mid-COPY*. With `RecyclingMethod::Fast` (no validation) the
    // next `pool.get()` would hand back that same connection, still in COPY-in mode, and
    // the subsequent `copy_in` would hang forever waiting for a response that never comes.
    // Two COPY sinks per topic: the tag table and its `<table>_geom` table. Both sets of pooled
    // `Object`s (`clients`) must be retained for the sinks' lifetime — see the deadpool pitfall
    // above; a recycled connection mid-COPY would hang the next `copy_in`.
    let n = tables.len();
    let mut clients = Vec::with_capacity(2 * n);
    let mut tag_sinks: Vec<std::pin::Pin<Box<tokio_postgres::CopyInSink<Bytes>>>> =
        Vec::with_capacity(n);
    let mut geom_sinks: Vec<std::pin::Pin<Box<tokio_postgres::CopyInSink<Bytes>>>> =
        Vec::with_capacity(n);
    for table in &tables {
        let tag_client = pool.get().await.context("getting topic DB connection")?;
        let tag_sink = tag_client.copy_in(&format!("COPY {table} {TAG_COPY_COLUMNS}")).await?;
        tag_sinks.push(Box::pin(tag_sink));
        clients.push(tag_client);

        let geom_client = pool.get().await.context("getting geom DB connection")?;
        let geom_sink = geom_client
            .copy_in(&format!("COPY {table}_geom {GEOM_COPY_COLUMNS}"))
            .await?;
        geom_sinks.push(Box::pin(geom_sink));
        clients.push(geom_client);
    }

    let mut tag_bufs: Vec<Vec<u8>>  = (0..n).map(|_| Vec::with_capacity(512 * 1024)).collect();
    let mut geom_bufs: Vec<Vec<u8>> = (0..n).map(|_| Vec::with_capacity(512 * 1024)).collect();
    let mut tag_counts: Vec<usize>  = vec![0; n];
    let mut geom_counts: Vec<usize> = vec![0; n];

    // Consumer-active timer: started on the first row (not sink setup), so it excludes the
    // reader's startup window. Compared against the reader's producer phases, this shows whether
    // the producer or the DB drain is the wall.
    let mut t_drain: Option<std::time::Instant> = None;
    while let Some(batch) = rx.recv().await {
        t_drain.get_or_insert_with(std::time::Instant::now);
        match batch {
            RowBatch::Tag(i, rows) => {
                tag_counts[i] += stream_rows(rows, &mut tag_bufs[i], tag_sinks[i].as_mut()).await?;
            }
            // Geometry is shared across topics; write the one built set to each surviving topic's
            // geom sink (bits set in `mask`), so no orphan geometry for topics that dropped the way.
            RowBatch::Geom(mask, rows) => {
                for i in 0..n {
                    if mask & (1 << i) != 0 {
                        geom_counts[i] +=
                            stream_geom_rows(&rows, &mut geom_bufs[i], geom_sinks[i].as_mut()).await?;
                    }
                }
            }
        }
    }

    for (i, sink) in tag_sinks.iter_mut().enumerate() {
        if !tag_bufs[i].is_empty() {
            sink.as_mut().send(Bytes::from(std::mem::take(&mut tag_bufs[i]))).await?;
        }
        sink.as_mut().finish().await?;
    }
    for (i, sink) in geom_sinks.iter_mut().enumerate() {
        if !geom_bufs[i].is_empty() {
            sink.as_mut().send(Bytes::from(std::mem::take(&mut geom_bufs[i]))).await?;
        }
        sink.as_mut().finish().await?;
    }
    if let Some(t) = t_drain {
        info!("[phase] DB drain + COPY finish (consumer-active): {:.1}s", t.elapsed().as_secs_f32());
    }

    producer.await.context("reader/processing task panicked")??;
    osm_pipeline::profile::report();

    for (i, table) in tables.iter().enumerate() {
        info!("Wrote {} tag rows → {}, {} geom rows → {}_geom", tag_counts[i], table, geom_counts[i], table);
    }
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
