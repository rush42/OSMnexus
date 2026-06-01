use osm_bikelanes::{config, db, engine, osm, processing, transform};

use anyhow::Context;
use bytes::Bytes;
use clap::Parser;
use futures::SinkExt;
use rayon::prelude::*;
use tracing::info;

use config::Config;
use db::{pool::build_pool, schema, writer};
use engine::topic::TopicSpec;
use osm::reader::read_highway_ways;
use processing::{process_way, WayOutput};
use transform::side_split::default_transformations;

const FLUSH_BYTES: usize = 512 * 1024;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt::init();

    let cfg = Config::parse();

    // Load the bikelane topic spec (runtime JSON — no recompile needed when changed).
    let topic_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("topics/bikelanes/topic.json");
    let topic: TopicSpec = serde_json::from_str(
        &std::fs::read_to_string(&topic_path)
            .with_context(|| format!("reading topic spec {}", topic_path.display()))?,
    )
    .context("parsing topic spec")?;
    info!("Loaded topic '{}' ({} osm fields, {} sanitizers)",
        topic.table, topic.osm_fields.len(), topic.sanitized_fields.len());

    info!("Connecting to database {}@{}/{}", cfg.db_user, cfg.db_host, cfg.db_name);
    let pool = build_pool(&cfg)?;
    let client_setup = pool.get().await.context("getting DB connection")?;

    info!("Setting up schema...");
    schema::create_tables(&client_setup).await?;
    if cfg.truncate {
        schema::truncate_tables(&client_setup).await?;
    }
    schema::drop_indexes(&client_setup).await?;
    drop(client_setup);

    info!("Reading PBF: {}", cfg.pbf_file);
    let t0 = std::time::Instant::now();
    let ways = read_highway_ways(&cfg.pbf_file)?;
    info!("{} highway ways loaded in {:.1}s", ways.len(), t0.elapsed().as_secs_f32());

    info!("Processing ways (streaming to DB)...");
    let t1 = std::time::Instant::now();
    let transformations = default_transformations();

    let (tx, mut rx) = tokio::sync::mpsc::channel::<WayOutput>(512);

    let process_task = tokio::task::spawn_blocking(move || {
        ways.par_iter().for_each(|way| {
            let _ = tx.blocking_send(process_way(way, &transformations, &topic));
        });
    });

    let client_bl = pool.get().await.context("getting bikelane DB connection")?;
    let client_rd = pool.get().await.context("getting road DB connection")?;

    let bikelane_sink = client_bl.copy_in(writer::COPY_BIKELANES).await?;
    let road_sink     = client_rd.copy_in(writer::COPY_ROADS).await?;
    let mut bikelane_sink = std::pin::pin!(bikelane_sink);
    let mut road_sink     = std::pin::pin!(road_sink);

    let mut bl_buf: Vec<u8> = Vec::with_capacity(FLUSH_BYTES);
    let mut rd_buf: Vec<u8> = Vec::with_capacity(FLUSH_BYTES);
    let mut bl_count = 0usize;
    let mut rd_count = 0usize;

    while let Some(output) = rx.recv().await {
        for row in output.bikelane_rows {
            writer::write_topic_csv_row(&mut bl_buf, &row)?;
            bl_count += 1;
            if bl_buf.len() >= FLUSH_BYTES {
                bikelane_sink.send(Bytes::from(std::mem::take(&mut bl_buf))).await?;
                bl_buf = Vec::with_capacity(FLUSH_BYTES);
            }
        }
        if let Some(row) = output.road_row {
            writer::write_road_csv_row(&mut rd_buf, &row)?;
            rd_count += 1;
            if rd_buf.len() >= FLUSH_BYTES {
                road_sink.send(Bytes::from(std::mem::take(&mut rd_buf))).await?;
                rd_buf = Vec::with_capacity(FLUSH_BYTES);
            }
        }
    }

    if !bl_buf.is_empty() { bikelane_sink.send(Bytes::from(bl_buf)).await?; }
    if !rd_buf.is_empty() { road_sink.send(Bytes::from(rd_buf)).await?; }
    bikelane_sink.finish().await?;
    road_sink.finish().await?;

    process_task.await.context("rayon processing panicked")?;

    info!(
        "Wrote {} bikelane rows, {} road rows in {:.1}s",
        bl_count, rd_count, t1.elapsed().as_secs_f32(),
    );

    info!("Creating indexes...");
    let client_idx = pool.get().await.context("getting index DB connection")?;
    schema::create_indexes(&client_idx).await?;

    info!("Done. Total: {:.1}s", t0.elapsed().as_secs_f32());
    Ok(())
}
