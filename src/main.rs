use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

use osm_pipeline::{config, db, engine, osm, output, processing};

use anyhow::Context;
use clap::Parser;
use deadpool_postgres::Pool;
use tokio::sync::mpsc;
use tracing::info;

use config::{Config, Output, Tiles};
use db::{pool::build_pool, schema, schema::GEOM_TABLE};
use engine::topic_runner::TopicRunner;
use osm::reader::stream_ways;
use osm::types::{ElementFilter, OsmWay, WayData};
use output::rows::{GeomRow, TopicRow, GEOM_COLUMNS, TAG_COLUMNS};
use output::writers::{copy_writer, csv_writer};
use processing::{classify_way, geom_rows_for};

/// Per-writer channel capacity (rows/batches buffered before the producer blocks).
const WRITER_CHAN_CAP: usize = 256;

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

    // Discover topics by scanning `topics/` (skipping `_`-prefixed dirs like `_shared`).
    // Drop a new `topics/<name>/` directory in and it's picked up — no code changes needed.
    let runners: Vec<TopicRunner> = TopicRunner::load_all()?;

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

    // Each topic declares which ways it needs (data-defined `element_filter` in topic.json,
    // defaulting to `highway`). The reader keeps any way matching any topic's filter.
    let filters: Vec<ElementFilter> = runners
        .iter()
        .map(|r| r.spec.element_filter.clone().unwrap_or_else(ElementFilter::highway))
        .collect();

    let n = tables.len();

    // Output backend. `w` = parallel writers per table: k sharded COPY connections for Postgres, a
    // single file writer for CSV. `pool` is `None` for CSV.
    let (pool, w): (Option<Pool>, usize) = match cfg.output {
        Output::Pg => {
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
            let k = cfg.db_writers.max(1);
            // Pool must supply every writer connection at once: k per tag table + k for geom.
            pool.resize((n + 1) * k + 2);
            info!("Postgres output · {k} COPY connection(s) per table");
            (Some(pool), k)
        }
        Output::Csv => {
            std::fs::create_dir_all(&cfg.out_dir)
                .with_context(|| format!("creating output dir {}", cfg.out_dir))?;
            info!("CSV output → {}/ (one file per tag table + {GEOM_TABLE}.csv)", cfg.out_dir);
            (None, 1)
        }
    };

    info!("Reading + processing PBF (streaming): {}", cfg.pbf_file);
    let t0 = std::time::Instant::now();

    // Spawn `w` writers per tag table + `w` for the shared geom table. For Postgres these are sharded
    // COPY connections (rows round-robined for k-way parallel serialization + ingest); for CSV, w=1,
    // so one buffered-file writer per table.
    let out_dir = PathBuf::from(&cfg.out_dir);
    let mut tag_senders: Vec<Vec<mpsc::Sender<Vec<TopicRow>>>> = Vec::with_capacity(n);
    let mut tag_handles: Vec<Vec<tokio::task::JoinHandle<anyhow::Result<usize>>>> = Vec::with_capacity(n);
    for table in &tables {
        let (mut ts, mut th) = (Vec::with_capacity(w), Vec::with_capacity(w));
        for _ in 0..w {
            let (tx, rx) = mpsc::channel::<Vec<TopicRow>>(WRITER_CHAN_CAP);
            let h = match cfg.output {
                Output::Pg => tokio::spawn(copy_writer::<TopicRow>(pool.clone().unwrap(), table.clone(), TAG_COLUMNS, rx)),
                Output::Csv => tokio::spawn(csv_writer::<TopicRow>(out_dir.join(format!("{table}.csv")), TAG_COLUMNS, rx)),
            };
            th.push(h);
            ts.push(tx);
        }
        tag_senders.push(ts);
        tag_handles.push(th);
    }
    let mut geom_senders: Vec<mpsc::Sender<Vec<GeomRow>>> = Vec::with_capacity(w);
    let mut geom_handles: Vec<tokio::task::JoinHandle<anyhow::Result<usize>>> = Vec::with_capacity(w);
    for _ in 0..w {
        let (tx, rx) = mpsc::channel::<Vec<GeomRow>>(WRITER_CHAN_CAP);
        let h = match cfg.output {
            Output::Pg => tokio::spawn(copy_writer::<GeomRow>(pool.clone().unwrap(), GEOM_TABLE.to_owned(), GEOM_COLUMNS, rx)),
            Output::Csv => tokio::spawn(csv_writer::<GeomRow>(out_dir.join(format!("{GEOM_TABLE}.csv")), GEOM_COLUMNS, rx)),
        };
        geom_handles.push(h);
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
                    let kk = tag_rr[i].fetch_add(1, Ordering::Relaxed) % w;
                    let _ = tag_senders[i][kk].blocking_send(rows);
                }
            }
            Some(())
        };
        let build_geom = |way: &OsmWay, _kept: ()| {
            let kk = geom_rr.fetch_add(1, Ordering::Relaxed) % w;
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

    match (cfg.output, cfg.create_index) {
        (Output::Pg, true) => {
            info!("Creating indexes...");
            let t_idx = std::time::Instant::now();
            schema::create_indexes(pool.as_ref().unwrap(), &table_refs).await?;
            info!("Index creation: {:.1}s", t_idx.elapsed().as_secs_f32());
        }
        (Output::Pg, false) => info!("Skipping index creation (pass --create-index to enable)"),
        (Output::Csv, _) => {}
    }

    if cfg.output == Output::Pg {
        match cfg.tiles {
            Tiles::None => {}
            Tiles::View => {
                info!("Creating tile views (<topic>_tiles)...");
                let client = pool.as_ref().unwrap().get().await?;
                db::tiles::create_tile_views(&client, &table_refs).await?;
            }
            Tiles::Materialized => {
                info!("Materializing tile tables + spatial index (<topic>_tiles)...");
                let t_tiles = std::time::Instant::now();
                db::tiles::materialize_tiles(pool.as_ref().unwrap(), &table_refs).await?;
                info!("Tile materialization: {:.1}s", t_tiles.elapsed().as_secs_f32());
            }
        }
    }

    info!("Done. Total: {:.1}s", t0.elapsed().as_secs_f32());
    Ok(())
}
