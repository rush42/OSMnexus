use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use osmnexus::{config, db, engine, osm, output, processing};

use anyhow::Context;
use clap::Parser;
use deadpool_postgres::Pool;
use rustc_hash::FxHashMap;
use tokio::sync::mpsc;
use tracing::info;

use config::{Config, Output};
use db::{
    pool::build_pool,
    schema,
    schema::{GeomTables, EDGE_TABLE, MEMBER_TABLE, NODE_TABLE, WAY_GEOM_TABLE},
};
use engine::runner::build_node_row;
use engine::topic_runner::TopicRunner;
use osm::reader::{stream_osm, Callbacks};
use osm::types::{ElementKind, NodeData, OsmWay, RelData, WayData};
use output::rows::{
    GeomRow, MemberRow, NodeRow, TopicRow, WayGeomRow, GEOM_COLUMNS, MEMBER_COLUMNS, NODE_COLUMNS,
    TAG_COLUMNS, WAY_GEOM_COLUMNS,
};
use output::writers::{copy_writer, csv_writer};
use processing::{classify_node, classify_relation, classify_way, geom_rows_for, way_geom_row_for};

/// Round-robin a batch of per-topic tag rows to each topic's `w` writers. Returns whether any topic
/// produced rows (i.e. the element was "kept" by some topic).
fn route_tag_rows(
    rows: Vec<Vec<TopicRow>>,
    senders: &[Vec<mpsc::Sender<Vec<TopicRow>>>],
    rr: &[AtomicUsize],
    w: usize,
) -> bool {
    let mut any = false;
    for (i, r) in rows.into_iter().enumerate() {
        if !r.is_empty() {
            any = true;
            let kk = rr[i].fetch_add(1, Ordering::Relaxed) % w;
            let _ = senders[i][kk].blocking_send(r);
        }
    }
    any
}

/// Per-writer channel capacity (rows/batches buffered before the producer blocks).
const WRITER_CHAN_CAP: usize = 256;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt::init();

    let cfg = Config::parse();
    osmnexus::profile::init_from_env();

    anyhow::ensure!(
        cfg.output == Output::Pg || !cfg.emit_relation_geometries,
        "--emit-relation-geometries requires --output pg (it merges member-way geometries with a \
         post-import SQL step, which needs a live database — CSV output has no table to merge from)"
    );

    osmnexus::traffic::set_left_hand_traffic(cfg.left_hand_traffic);
    if cfg.left_hand_traffic {
        info!("Left-hand traffic: forward/backward directed tags read from the opposite side");
    }

    // Size the rayon pool (CPU-bound decode/stream). `0` = rayon's default (logical CPU count).
    if cfg.threads > 0 {
        rayon::ThreadPoolBuilder::new()
            .num_threads(cfg.threads)
            .build_global()
            .context("configuring rayon thread pool")?;
        info!("rayon thread pool capped at {} threads", cfg.threads);
    }

    // Select the config directory (a self-contained set of topics + its `_shared/` library) and
    // discover its topics (skipping `_`-prefixed dirs). Drop a new `<config>/<name>/` dir in and
    // it's picked up — no code changes needed.
    osmnexus::paths::set_config_root(cfg.config_dir.clone());
    info!("Config directory: {}", cfg.config_dir.display());
    let runners: Vec<TopicRunner> = TopicRunner::load_all(cfg.tree_max_depth)?;

    let tables: Vec<String> = runners.iter().map(|r| r.table().to_owned()).collect();
    let table_refs: Vec<&str> = tables.iter().map(String::as_str).collect();

    for r in &runners {
        info!(
            "Loaded topic '{}' ({} categories, {} osm fields, {} sanitizers, {} derivers)",
            r.table(),
            r.categories.values().map(|c| c.categories.len()).sum::<usize>(),
            r.spec.osm_fields.len(),
            r.sanitizer_fields.len(),
            r.topic_derivers.len()
        );
    }

    // Which passes are active: gated by whether any topic declares node/relation categories (a
    // `topics/<t>/{node,relation}/` subfolder). Selection is the decision-tree classifier itself —
    // there is no coarse element filter; an element is kept iff some topic categorizes it. Node
    // coords are always collected regardless (needed for the always-on `nodes` table).
    let has_relations = runners.iter().any(|r| r.has_kind(ElementKind::Relation));
    let has_nodes = runners.iter().any(|r| r.has_kind(ElementKind::Node));
    let geom_tables = GeomTables { way: cfg.emit_way_geometries };

    let n = tables.len();
    // Extra sharded-writer tables beyond the per-topic tag tables: edges + members + nodes, plus the
    // optional way geom table.
    let extra_tables = 3 + geom_tables.way as usize;

    // Output backend. `w` = parallel writers per table: k sharded COPY connections for Postgres, a
    // single file writer for CSV. `pool` is `None` for CSV.
    let (pool, w): (Option<Pool>, usize) = match cfg.output {
        Output::Pg => {
            info!("Connecting to database {}@{}/{}", cfg.db_user, cfg.db_host, cfg.db_name);
            let pool = build_pool(&cfg)?;
            let client_setup = pool.get().await.context("getting DB connection")?;
            info!("Setting up schema...");
            schema::create_tables(&client_setup, &table_refs, geom_tables).await?;
            if cfg.truncate {
                schema::truncate_tables(&client_setup, &table_refs, geom_tables).await?;
            }
            schema::drop_indexes(&client_setup, &table_refs, geom_tables).await?;
            drop(client_setup);
            let k = cfg.db_writers.max(1);
            // Pool must supply every writer connection at once: k per tag table + k per extra table.
            pool.resize((n + extra_tables) * k + 2);
            info!("Postgres output · {k} COPY connection(s) per table");
            (Some(pool), k)
        }
        Output::Csv => {
            std::fs::create_dir_all(&cfg.out_dir)
                .with_context(|| format!("creating output dir {}", cfg.out_dir))?;
            info!("CSV output → {}/ (one file per tag table + {EDGE_TABLE}.csv)", cfg.out_dir);
            (None, 1)
        }
        Output::GeoJson => {
            std::fs::create_dir_all(&cfg.out_dir)
                .with_context(|| format!("creating output dir {}", cfg.out_dir))?;
            info!("GeoJSON output → {}/ (one {{topic}}.geojson FeatureCollection per topic)", cfg.out_dir);
            (None, 1)
        }
    };

    info!("Reading + processing PBF (streaming): {}", cfg.pbf_file);
    let t0 = std::time::Instant::now();

    // Spawn `w` writers per tag table + `w` for each shared table. For Postgres these are sharded
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
                Output::Csv | Output::GeoJson => tokio::spawn(csv_writer::<TopicRow>(out_dir.join(format!("{table}.csv")), TAG_COLUMNS, rx)),
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
            Output::Pg => tokio::spawn(copy_writer::<GeomRow>(pool.clone().unwrap(), EDGE_TABLE.to_owned(), GEOM_COLUMNS, rx)),
            Output::Csv | Output::GeoJson => tokio::spawn(csv_writer::<GeomRow>(out_dir.join(format!("{EDGE_TABLE}.csv")), GEOM_COLUMNS, rx)),
        };
        geom_handles.push(h);
        geom_senders.push(tx);
    }
    let mut member_senders: Vec<mpsc::Sender<Vec<MemberRow>>> = Vec::with_capacity(w);
    let mut member_handles: Vec<tokio::task::JoinHandle<anyhow::Result<usize>>> = Vec::with_capacity(w);
    for _ in 0..w {
        let (tx, rx) = mpsc::channel::<Vec<MemberRow>>(WRITER_CHAN_CAP);
        let h = match cfg.output {
            Output::Pg => tokio::spawn(copy_writer::<MemberRow>(pool.clone().unwrap(), MEMBER_TABLE.to_owned(), MEMBER_COLUMNS, rx)),
            Output::Csv | Output::GeoJson => tokio::spawn(csv_writer::<MemberRow>(out_dir.join(format!("{MEMBER_TABLE}.csv")), MEMBER_COLUMNS, rx)),
        };
        member_handles.push(h);
        member_senders.push(tx);
    }
    // Optional: whole-way linestrings (`--emit-way-geometries`).
    let mut way_geom_senders: Vec<mpsc::Sender<Vec<WayGeomRow>>> = Vec::with_capacity(w);
    let mut way_geom_handles: Vec<tokio::task::JoinHandle<anyhow::Result<usize>>> = Vec::with_capacity(w);
    if cfg.emit_way_geometries {
        for _ in 0..w {
            let (tx, rx) = mpsc::channel::<Vec<WayGeomRow>>(WRITER_CHAN_CAP);
            let h = match cfg.output {
                Output::Pg => tokio::spawn(copy_writer::<WayGeomRow>(pool.clone().unwrap(), WAY_GEOM_TABLE.to_owned(), WAY_GEOM_COLUMNS, rx)),
                Output::Csv | Output::GeoJson => tokio::spawn(csv_writer::<WayGeomRow>(out_dir.join(format!("{WAY_GEOM_TABLE}.csv")), WAY_GEOM_COLUMNS, rx)),
            };
            way_geom_handles.push(h);
            way_geom_senders.push(tx);
        }
    }
    // Graph-vertex table (always emitted — see `NODE_TABLE`).
    let mut node_senders: Vec<mpsc::Sender<Vec<NodeRow>>> = Vec::with_capacity(w);
    let mut node_handles: Vec<tokio::task::JoinHandle<anyhow::Result<usize>>> = Vec::with_capacity(w);
    for _ in 0..w {
        let (tx, rx) = mpsc::channel::<Vec<NodeRow>>(WRITER_CHAN_CAP);
        let h = match cfg.output {
            Output::Pg => tokio::spawn(copy_writer::<NodeRow>(pool.clone().unwrap(), NODE_TABLE.to_owned(), NODE_COLUMNS, rx)),
            Output::Csv | Output::GeoJson => tokio::spawn(csv_writer::<NodeRow>(out_dir.join(format!("{NODE_TABLE}.csv")), NODE_COLUMNS, rx)),
        };
        node_handles.push(h);
        node_senders.push(tx);
    }

    // Producer: the reader decodes the PBF once and drives the callbacks below. Runs on the blocking
    // pool (CPU-bound rayon work). Dropping the producer drops all senders, closing the writer
    // channels.
    let pbf_file = cfg.pbf_file.clone();
    let emit_way_geometries = cfg.emit_way_geometries;
    // Shared, thread-safe state captured by the reader's callbacks (called from rayon workers).
    let runners = Arc::new(runners);
    let tag_senders = Arc::new(tag_senders);
    let geom_senders = Arc::new(geom_senders);
    let member_senders = Arc::new(member_senders);
    let way_geom_senders = Arc::new(way_geom_senders);
    let node_senders = Arc::new(node_senders);
    let tag_rr: Arc<Vec<AtomicUsize>> = Arc::new((0..n).map(|_| AtomicUsize::new(0)).collect());
    let geom_rr = Arc::new(AtomicUsize::new(0));
    let member_rr = Arc::new(AtomicUsize::new(0));
    let node_rr = Arc::new(AtomicUsize::new(0));
    let producer = tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        // Ways pass: emit tag rows; keep the way if some topic categorized it OR it is a relation
        // member (`in_keep`). Payload is `()` — geometry is topic-independent (one shared table).
        let classify_way_cb = {
            let (runners, tag_senders, tag_rr) = (runners.clone(), tag_senders.clone(), tag_rr.clone());
            move |wd: &WayData, in_keep: bool| -> Option<()> {
                let out = classify_way(&runners, wd);
                let kept_by_topic = route_tag_rows(out.topic_rows, &tag_senders, &tag_rr, w);
                (kept_by_topic || in_keep).then_some(())
            }
        };
        // Relations pass: emit relation tag rows + `relation_members` links; kept iff some topic
        // categorized the relation. Member ways of a kept relation join the graph keep set.
        let classify_rel_cb = {
            let (runners, tag_senders, tag_rr) = (runners.clone(), tag_senders.clone(), tag_rr.clone());
            let (member_senders, member_rr) = (member_senders.clone(), member_rr.clone());
            move |rd: &RelData| -> bool {
                let kept = route_tag_rows(classify_relation(&runners, rd), &tag_senders, &tag_rr, w);
                if kept && !rd.member_ways.is_empty() {
                    let links: Vec<MemberRow> = rd
                        .member_ways
                        .iter()
                        .map(|&wid| MemberRow { relation_osm_id: rd.id, way_osm_id: wid })
                        .collect();
                    let kk = member_rr.fetch_add(1, Ordering::Relaxed) % w;
                    let _ = member_senders[kk].blocking_send(links);
                }
                kept
            }
        };
        // Nodes pass: emit node tag rows; a node is "selected" (forced cut point) iff some topic
        // categorized it.
        let classify_node_cb = {
            let (runners, tag_senders, tag_rr) = (runners.clone(), tag_senders.clone(), tag_rr.clone());
            move |nd: &NodeData| -> bool { route_tag_rows(classify_node(&runners, nd), &tag_senders, &tag_rr, w) }
        };
        // Graph-vertex pass: emit the `nodes` table rows, once, for every graph vertex (see
        // `assign_node_ids`) — always runs, replacing the old `--emit-node-geometries` flag.
        let build_nodes_cb = {
            let (node_senders, node_rr) = (node_senders.clone(), node_rr.clone());
            move |nodes: Vec<(i64, i64, f32, f32)>| {
                for chunk in nodes.chunks(4096) {
                    let rows: Vec<NodeRow> = chunk
                        .iter()
                        .map(|&(id, osm_id, lon, lat)| build_node_row(id, osm_id, lon as f64, lat as f64))
                        .collect();
                    let kk = node_rr.fetch_add(1, Ordering::Relaxed) % w;
                    let _ = node_senders[kk].blocking_send(rows);
                }
            }
        };
        // Geometry pass: write the resolved way's graph edges to the shared table, plus the
        // whole-way linestring when `--emit-way-geometries` is set.
        let build_geom_cb = {
            let (geom_senders, geom_rr) = (geom_senders.clone(), geom_rr.clone());
            let way_geom_senders = way_geom_senders.clone();
            move |way: &OsmWay, _kept: (), node_ids: &FxHashMap<i64, i64>| {
                let kk = geom_rr.fetch_add(1, Ordering::Relaxed) % w;
                let _ = geom_senders[kk].blocking_send(geom_rows_for(way, node_ids));
                if emit_way_geometries {
                    let _ = way_geom_senders[kk].blocking_send(vec![way_geom_row_for(way)]);
                }
            }
        };
        stream_osm(
            &pbf_file,
            Callbacks {
                has_relations,
                classify_rel: classify_rel_cb,
                classify_way: classify_way_cb,
                has_nodes,
                classify_node: classify_node_cb,
                build_geom: build_geom_cb,
                build_nodes: build_nodes_cb,
            },
        )
    });

    // Await the producer first: it drops the senders, closing every writer channel so the writer
    // tasks drain their tails, finish the COPY, and return their counts.
    producer.await.context("reader/processing task panicked")??;
    osmnexus::profile::report();

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
    let mut member_count = 0usize;
    for h in member_handles {
        member_count += h.await.context("member writer panicked")??;
    }
    let mut way_geom_count = 0usize;
    for h in way_geom_handles {
        way_geom_count += h.await.context("way-geom writer panicked")??;
    }
    let mut node_count = 0usize;
    for h in node_handles {
        node_count += h.await.context("node writer panicked")??;
    }

    for (i, table) in tables.iter().enumerate() {
        info!("Wrote {} tag rows → {}", tag_counts[i], table);
    }
    info!("Wrote {geom_count} edge rows → {EDGE_TABLE}");
    info!("Wrote {member_count} relation-member links → {MEMBER_TABLE}");
    info!("Wrote {node_count} node rows → {NODE_TABLE}");
    if cfg.emit_way_geometries {
        info!("Wrote {way_geom_count} way rows → {WAY_GEOM_TABLE}");
    }
    info!("Read + process time: {:.1}s", t0.elapsed().as_secs_f32());

    match (cfg.output, cfg.create_index) {
        (Output::Pg, true) => {
            info!("Creating indexes...");
            let t_idx = std::time::Instant::now();
            schema::create_indexes(pool.as_ref().unwrap(), &table_refs, geom_tables).await?;
            info!("Index creation: {:.1}s", t_idx.elapsed().as_secs_f32());
        }
        (Output::Pg, false) => info!("Skipping index creation (pass --create-index to enable)"),
        (Output::Csv, _) | (Output::GeoJson, _) => {}
    }

    if cfg.output == Output::Pg {
        if let Some(mode) = cfg.topic_edges {
            let client = pool.as_ref().unwrap().get().await?;
            for table in &tables {
                info!("Materializing graph edges → {table}_edge");
                db::topic_edges::materialize(&client, table, mode, cfg.create_index).await?;
            }
        }
    }

    if cfg.output == Output::GeoJson {
        info!("Building GeoJSON from CSV output...");
        output::geojson::write_geojson_from_csv(&out_dir, &tables)?;
        for table in &tables {
            info!("Wrote {}/{table}.geojson", cfg.out_dir);
        }
    }

    if cfg.output == Output::Pg && cfg.emit_relation_geometries {
        info!("Materializing relation geometries...");
        let t_rel = std::time::Instant::now();
        let client = pool.as_ref().unwrap().get().await?;
        db::relations::materialize_relation_geometries(&client, cfg.emit_way_geometries).await?;
        info!("Relation geometry materialization: {:.1}s", t_rel.elapsed().as_secs_f32());
    }

    info!("Done. Total: {:.1}s", t0.elapsed().as_secs_f32());
    Ok(())
}
