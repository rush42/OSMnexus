use osm_bikelanes::{config, db, engine, osm, processing};

use anyhow::Context;
use bytes::Bytes;
use clap::Parser;
use futures::SinkExt;
use rayon::prelude::*;
use tracing::info;

use config::Config;
use db::{pool::build_pool, schema};
use engine::topic_runner::{stream_rows, TopicRunner};
use osm::reader::read_highway_ways;
use processing::{process_way, WayOutput};

const COPY_COLUMNS: &str =
    "(osm_id, osm_type, id, osm, sanitized, derived, private, meta, geom, minzoom) FROM STDIN (FORMAT CSV)";

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
            "Loaded topic '{}' ({} categories, {} osm fields, {} sanitizers)",
            r.table(),
            r.categories.categories.len(),
            r.spec.osm_fields.len(),
            r.spec.sanitized_fields.len()
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

    info!("Reading PBF: {}", cfg.pbf_file);
    let t0 = std::time::Instant::now();
    let ways = read_highway_ways(&cfg.pbf_file)?;
    info!("{} highway ways loaded in {:.1}s", ways.len(), t0.elapsed().as_secs_f32());

    info!(
        "Processing {} topics across {} ways (streaming to DB)...",
        runners.len(),
        ways.len()
    );
    let t1 = std::time::Instant::now();

    let (tx, mut rx) = tokio::sync::mpsc::channel::<WayOutput>(512);

    let process_task = tokio::task::spawn_blocking(move || {
        ways.par_iter()
            .for_each(|way| { let _ = tx.blocking_send(process_way(way, &runners)); });
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

    for (i, mut sink) in sinks.iter_mut().enumerate() {
        if !bufs[i].is_empty() {
            sink.as_mut().send(Bytes::from(std::mem::take(&mut bufs[i]))).await?;
        }
        sink.as_mut().finish().await?;
    }

    process_task.await.context("rayon processing panicked")?;

    for (i, table) in tables.iter().enumerate() {
        info!("Wrote {} rows → {}", counts[i], table);
    }
    info!("Processing time: {:.1}s", t1.elapsed().as_secs_f32());

    info!("Creating indexes...");
    let client_idx = pool.get().await.context("getting index DB connection")?;
    schema::create_indexes(&client_idx, &table_refs).await?;

    info!("Done. Total: {:.1}s", t0.elapsed().as_secs_f32());
    Ok(())
}
