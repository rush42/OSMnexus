use osm_pipeline::{config, db, engine, osm, processing};

use anyhow::Context;
use bytes::Bytes;
use clap::Parser;
use futures::SinkExt;
use tracing::info;

use config::Config;
use db::{pool::build_pool, schema};
use engine::topic_runner::{stream_rows, TopicRunner};
use osm::reader::stream_ways;
use osm::types::{ElementFilter, NodeIndex};
use processing::{process_way, WayOutput};

const COPY_COLUMNS: &str =
    "(osm_id, osm_type, id, osm, derived, private, meta, geom, minzoom) FROM STDIN (FORMAT CSV)";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt::init();

    let cfg = Config::parse();

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

    let (tx, mut rx) = tokio::sync::mpsc::channel::<WayOutput>(512);

    // Producer: stream ways straight from the PBF and process each into rows, fed to the COPY
    // consumer below. Runs on the blocking pool because the reader is CPU-bound rayon work.
    // Ways are never materialized — peak memory tracks the node coordinates, not the way count.
    let pbf_file = cfg.pbf_file.clone();
    let find_intersections = cfg.find_intersections;
    let producer = tokio::task::spawn_blocking(move || -> anyhow::Result<Option<NodeIndex>> {
        stream_ways(&pbf_file, &filters, find_intersections, |way| {
            let _ = tx.blocking_send(process_way(way, &runners));
        })
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
    let n = tables.len();
    let mut clients = Vec::with_capacity(n);
    let mut sinks: Vec<std::pin::Pin<Box<tokio_postgres::CopyInSink<Bytes>>>> =
        Vec::with_capacity(n);
    for table in &tables {
        let client = pool.get().await.context("getting topic DB connection")?;
        let sink = client.copy_in(&format!("COPY {table} {COPY_COLUMNS}")).await?;
        sinks.push(Box::pin(sink));
        clients.push(client); // keep the connection out of the pool until COPY finishes
    }

    let mut bufs: Vec<Vec<u8>>  = (0..n).map(|_| Vec::with_capacity(512 * 1024)).collect();
    let mut counts: Vec<usize>  = vec![0; n];

    while let Some(WayOutput(topic_rows)) = rx.recv().await {
        for (i, rows) in topic_rows.into_iter().enumerate() {
            counts[i] += stream_rows(rows, &mut bufs[i], sinks[i].as_mut()).await?;
        }
    }

    for (i, sink) in sinks.iter_mut().enumerate() {
        if !bufs[i].is_empty() {
            sink.as_mut().send(Bytes::from(std::mem::take(&mut bufs[i]))).await?;
        }
        sink.as_mut().finish().await?;
    }

    // NodeIndex (coords + per-node use counts) is returned only with --find-intersections;
    // otherwise it's dropped inside the reader, keeping memory proportional to node coords.
    let node_index = producer.await.context("reader/processing task panicked")??;
    if let Some(ni) = &node_index {
        let intersections = ni.use_counts.values().filter(|&&c| c >= 2).count();
        info!(
            "Node index ready: {} referenced nodes, {} intersections (≥2 ways)",
            ni.use_counts.len(),
            intersections
        );
    }

    for (i, table) in tables.iter().enumerate() {
        info!("Wrote {} rows → {}", counts[i], table);
    }
    info!("Read + process time: {:.1}s", t0.elapsed().as_secs_f32());

    info!("Creating indexes...");
    let client_idx = pool.get().await.context("getting index DB connection")?;
    schema::create_indexes(&client_idx, &table_refs).await?;

    info!("Done. Total: {:.1}s", t0.elapsed().as_secs_f32());
    Ok(())
}
