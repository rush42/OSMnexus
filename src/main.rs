use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use osmnexus::{config, db, geom, osm, output, processing, topic};

use anyhow::Context;
use clap::Parser;
use deadpool_postgres::Pool;
use rustc_hash::{FxHashMap, FxHashSet};
use tokio::sync::mpsc;
use tracing::info;

use config::{Config, Output};
use db::{
    pool::build_pool,
    schema,
    schema::{EDGE_TABLE, MEMBER_TABLE, NODE_TABLE},
};
use geom::builders::{build_node_point_row, build_node_row, build_way_point_row, build_way_polygon_row};
use geom::rows::{
    GeomRow, NodeRow, PointRow, PolygonRow, WayGeomRow, GEOM_COLUMNS, NODE_COLUMNS, POINT_COLUMNS,
    POLYGON_COLUMNS, WAY_GEOM_COLUMNS,
};
use topic::spec::GeometryShape;
use topic::TopicRunner;
use osm::reader::{stream_osm, Callbacks};
use osm::types::{ElementKind, NodeData, OsmWay, RelData, WayData};
use output::rows::{CsvRow, MemberRow, TopicRow, MEMBER_COLUMNS, TAG_COLUMNS};
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

/// Fan a way-shaped row (whole-way linestring or polygon) out to every topic that both declared
/// the matching shape and kept this way (`mask` — see `ClassifyOutput`). `topics[i]` is the bit
/// index into `mask` for `senders[i]`/`rr[i]` — `topics` is already precomputed as exactly the
/// topics wanting this shape, so no further per-topic gate is needed here.
fn route_shape_row<T: Clone>(
    row: &T,
    mask: u32,
    topics: &[usize],
    senders: &[Vec<mpsc::Sender<Vec<T>>>],
    rr: &[AtomicUsize],
    w: usize,
) {
    for (i, &topic_idx) in topics.iter().enumerate() {
        if mask & (1 << topic_idx) != 0 {
            let kk = rr[i].fetch_add(1, Ordering::Relaxed) % w;
            let _ = senders[i][kk].blocking_send(vec![row.clone()]);
        }
    }
}

/// Like `route_shape_row`, but for `point` rows specifically: `point_topics` is the union of every
/// topic wanting `point` on *either* `node` or `way` (they share one `{table}_point` table), so an
/// extra `eligible[i]` gate (computed once by the caller — `wants(kind, Point)` for whichever kind
/// is calling) is needed on top of the keep-`mask` check, unlike `route_shape_row`'s single-kind
/// shapes (`line`/`polygon`/`graph`), where `topics` alone is already kind-specific.
fn route_point_row(
    row: &PointRow,
    mask: u32,
    point_topics: &[usize],
    eligible: &[bool],
    senders: &[Vec<mpsc::Sender<Vec<PointRow>>>],
    rr: &[AtomicUsize],
    w: usize,
) {
    for (i, &topic_idx) in point_topics.iter().enumerate() {
        if eligible[i] && mask & (1 << topic_idx) != 0 {
            let kk = rr[i].fetch_add(1, Ordering::Relaxed) % w;
            let _ = senders[i][kk].blocking_send(vec![row.clone()]);
        }
    }
}

/// Per-writer channel capacity (rows/batches buffered before the producer blocks).
const WRITER_CHAN_CAP: usize = 256;

/// Write an already-fully-built (small) batch of rows to `table` in one shot — used for relation
/// geometry, which is resolved entirely in memory *after* the main streaming pass (see
/// `geom::relation`), so it has no ongoing channel to shard across `w` writers like the
/// streaming tables above; one connection/file is plenty for what's typically a small dataset.
async fn write_rows_once<R: CsvRow + Send + Sync + 'static>(
    output: Output,
    pool: &Option<Pool>,
    out_dir: &std::path::Path,
    table: &str,
    columns: &'static str,
    rows: Vec<R>,
) -> anyhow::Result<usize> {
    let (tx, rx) = mpsc::channel(1);
    let handle = match output {
        Output::Pg => tokio::spawn(copy_writer::<R>(pool.clone().unwrap(), table.to_owned(), columns, rx)),
        Output::Csv | Output::GeoJson => {
            tokio::spawn(csv_writer::<R>(out_dir.join(format!("{table}.csv")), columns, rx))
        }
    };
    tx.send(rows).await.ok();
    drop(tx);
    handle.await.context("relation-geometry writer panicked")?
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt::init();
    osmnexus::profiling::init_from_env();

    let cfg = Config::parse();

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

    // Select the config directory (a self-contained set of topics + a shared config-root library) and
    // discover its topics (skipping `_`-prefixed dirs). Drop a new `<config>/<name>/` dir in and
    // it's picked up — no code changes needed.
    osmnexus::paths::set_config_root(cfg.config_dir.clone());
    info!("Config directory: {}", cfg.config_dir.display());
    let runners: Vec<TopicRunner> = TopicRunner::load_all(cfg.tree_max_depth)?;

    let tables: Vec<String> = runners.iter().map(|r| r.table().to_owned()).collect();
    let table_refs: Vec<&str> = tables.iter().map(String::as_str).collect();

    for r in &runners {
        info!(
            "Loaded topic '{}' ({} categories, {} outputs)",
            r.table(),
            r.categories.values().map(|c| c.categories.len()).sum::<usize>(),
            r.spec.outputs.len(),
        );
        for (kind, cats) in &r.categories {
            let s = cats.tree.stats();
            info!(
                "  {:?} tree: {} leaves, {} branches, max depth {}, avg leaf depth {:.1}, avg leaf size {:.1}",
                kind, s.leaf_count, s.branch_count, s.max_depth, s.avg_leaf_depth(), s.avg_leaf_size(),
            );
        }
    }

    // Which passes are active: gated by whether any topic declares node/relation categories (a
    // `topics/<t>/{node,relation}/` subfolder). Selection is the decision-tree classifier itself —
    // there is no coarse element filter; an element is kept iff some topic categorizes it. Node
    // coords are always collected regardless (needed for the always-on `nodes` table).
    let has_relations = runners.iter().any(|r| r.has_kind(ElementKind::Relation));
    let has_nodes = runners.iter().any(|r| r.has_kind(ElementKind::Node));

    // Topics opting into geometry outputs (see `TopicSpec::geometry`) — replaces the old global
    // `--emit-way-geometries`/`--emit-relation-geometries`/unconditional `--topic-edges` flags.
    let way_linestring_topics: Vec<usize> =
        (0..runners.len()).filter(|&i| runners[i].wants(ElementKind::Way, GeometryShape::Line)).collect();
    // `point` is shared by `node` and `way` (a node's own coordinate, or a way's centroid) — one
    // `{table}_point` table per topic wanting either. `polygon` is way-only for now (a closed way's
    // own ring — multipolygon/inner-ring assembly from relation member roles isn't implemented).
    let way_polygon_topics: Vec<usize> =
        (0..runners.len()).filter(|&i| runners[i].wants(ElementKind::Way, GeometryShape::Polygon)).collect();
    let point_topics: Vec<usize> = (0..runners.len())
        .filter(|&i| {
            runners[i].wants(ElementKind::Way, GeometryShape::Point)
                || runners[i].wants(ElementKind::Node, GeometryShape::Point)
        })
        .collect();
    let way_point_eligible: Vec<bool> =
        point_topics.iter().map(|&i| runners[i].wants(ElementKind::Way, GeometryShape::Point)).collect();
    let node_point_eligible: Vec<bool> =
        point_topics.iter().map(|&i| runners[i].wants(ElementKind::Node, GeometryShape::Point)).collect();
    // Relation `line`/`point`: built in-process (see `geom::relation`), independent of the
    // ways/geometry passes — no Pg-only restriction, unlike the old SQL post-processing step.
    let relation_line_topics: Vec<usize> =
        (0..runners.len()).filter(|&i| runners[i].wants(ElementKind::Relation, GeometryShape::Line)).collect();
    let relation_point_topics: Vec<usize> =
        (0..runners.len()).filter(|&i| runners[i].wants(ElementKind::Relation, GeometryShape::Point)).collect();
    let relation_polygon_topics: Vec<usize> =
        (0..runners.len()).filter(|&i| runners[i].wants(ElementKind::Relation, GeometryShape::Polygon)).collect();
    // Bitmask of every topic wanting *any* relation geometry — `classify_rel_cb`'s cheap gate for
    // whether a kept relation is even worth recording for the post-stream relation-geometry step.
    let rel_geom_mask: u32 = relation_line_topics
        .iter()
        .chain(&relation_point_topics)
        .chain(&relation_polygon_topics)
        .fold(0u32, |m, &i| m | (1 << i));

    // Every non-tag-table geometry table this run needs (see `schema::GeomTableShape`'s own doc) —
    // one consolidated list instead of a parallel `&[&str]` per shape.
    let geom_tables: Vec<(String, schema::GeomTableShape)> = way_linestring_topics
        .iter()
        .map(|&i| (schema::way_geom_table(table_refs[i]), schema::GeomTableShape::LineString))
        .chain(way_polygon_topics.iter().map(|&i| (schema::polygon_table(table_refs[i]), schema::GeomTableShape::Polygon)))
        .chain(point_topics.iter().map(|&i| (schema::point_table(table_refs[i]), schema::GeomTableShape::Point)))
        .chain(relation_line_topics.iter().map(|&i| (schema::relation_geom_table(table_refs[i]), schema::GeomTableShape::LineString)))
        .chain(relation_point_topics.iter().map(|&i| (schema::relation_point_table(table_refs[i]), schema::GeomTableShape::Point)))
        .chain(relation_polygon_topics.iter().map(|&i| (schema::relation_polygon_table(table_refs[i]), schema::GeomTableShape::Polygon)))
        .collect();
    let way_geom_table_refs: Vec<&str> = way_linestring_topics.iter().map(|&i| table_refs[i]).collect();
    let polygon_table_refs: Vec<&str> = way_polygon_topics.iter().map(|&i| table_refs[i]).collect();
    let point_table_refs: Vec<&str> = point_topics.iter().map(|&i| table_refs[i]).collect();
    let relation_line_table_refs: Vec<&str> = relation_line_topics.iter().map(|&i| table_refs[i]).collect();
    let relation_point_table_refs: Vec<&str> = relation_point_topics.iter().map(|&i| table_refs[i]).collect();
    let relation_polygon_table_refs: Vec<&str> = relation_polygon_topics.iter().map(|&i| table_refs[i]).collect();

    let n = tables.len();
    // Extra sharded-writer tables beyond the per-topic tag tables: edges + members + nodes, plus one
    // per topic wanting a way-shaped geometry table (relation geometry writes separately, after the
    // main streaming pass, so it isn't part of this pool-sizing count — see below).
    let extra_tables = 3 + way_linestring_topics.len() + way_polygon_topics.len() + point_topics.len();

    // Output backend. `w` = parallel writers per table: k sharded COPY connections for Postgres, a
    // single file writer for CSV. `pool` is `None` for CSV.
    let (pool, w): (Option<Pool>, usize) = match cfg.output {
        Output::Pg => {
            info!("Connecting to database {}@{}/{}", cfg.db_user, cfg.db_host, cfg.db_name);
            let pool = build_pool(&cfg)?;
            let client_setup = pool.get().await.context("getting DB connection")?;
            info!("Setting up schema...");
            schema::create_tables(&client_setup, &table_refs, &geom_tables).await?;
            if cfg.truncate {
                schema::truncate_tables(&client_setup, &table_refs, &geom_tables).await?;
            }
            schema::drop_indexes(&client_setup, &table_refs, &geom_tables).await?;
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
    // One whole-way-linestring table per topic declaring `"geometry": { "way": ["linestring"] }`.
    let mut way_geom_senders: Vec<Vec<mpsc::Sender<Vec<WayGeomRow>>>> = Vec::with_capacity(way_linestring_topics.len());
    let mut way_geom_handles: Vec<Vec<tokio::task::JoinHandle<anyhow::Result<usize>>>> = Vec::with_capacity(way_linestring_topics.len());
    for &table_name in &way_geom_table_refs {
        let way_geom_table = schema::way_geom_table(table_name);
        let (mut ts, mut th) = (Vec::with_capacity(w), Vec::with_capacity(w));
        for _ in 0..w {
            let (tx, rx) = mpsc::channel::<Vec<WayGeomRow>>(WRITER_CHAN_CAP);
            let h = match cfg.output {
                Output::Pg => tokio::spawn(copy_writer::<WayGeomRow>(pool.clone().unwrap(), way_geom_table.clone(), WAY_GEOM_COLUMNS, rx)),
                Output::Csv | Output::GeoJson => tokio::spawn(csv_writer::<WayGeomRow>(out_dir.join(format!("{way_geom_table}.csv")), WAY_GEOM_COLUMNS, rx)),
            };
            th.push(h);
            ts.push(tx);
        }
        way_geom_senders.push(ts);
        way_geom_handles.push(th);
    }
    // One polygon table per topic declaring `"geometry": { "way": ["polygon"] }`.
    let mut polygon_senders: Vec<Vec<mpsc::Sender<Vec<PolygonRow>>>> = Vec::with_capacity(way_polygon_topics.len());
    let mut polygon_handles: Vec<Vec<tokio::task::JoinHandle<anyhow::Result<usize>>>> = Vec::with_capacity(way_polygon_topics.len());
    for &table_name in &polygon_table_refs {
        let polygon_table = schema::polygon_table(table_name);
        let (mut ts, mut th) = (Vec::with_capacity(w), Vec::with_capacity(w));
        for _ in 0..w {
            let (tx, rx) = mpsc::channel::<Vec<PolygonRow>>(WRITER_CHAN_CAP);
            let h = match cfg.output {
                Output::Pg => tokio::spawn(copy_writer::<PolygonRow>(pool.clone().unwrap(), polygon_table.clone(), POLYGON_COLUMNS, rx)),
                Output::Csv | Output::GeoJson => tokio::spawn(csv_writer::<PolygonRow>(out_dir.join(format!("{polygon_table}.csv")), POLYGON_COLUMNS, rx)),
            };
            th.push(h);
            ts.push(tx);
        }
        polygon_senders.push(ts);
        polygon_handles.push(th);
    }
    // One point table per topic declaring `"geometry": { "node"|"way": ["point"] }` (shared between
    // both kinds — see `point_topics`'s own doc above).
    let mut point_senders: Vec<Vec<mpsc::Sender<Vec<PointRow>>>> = Vec::with_capacity(point_topics.len());
    let mut point_handles: Vec<Vec<tokio::task::JoinHandle<anyhow::Result<usize>>>> = Vec::with_capacity(point_topics.len());
    for &table_name in &point_table_refs {
        let point_table = schema::point_table(table_name);
        let (mut ts, mut th) = (Vec::with_capacity(w), Vec::with_capacity(w));
        for _ in 0..w {
            let (tx, rx) = mpsc::channel::<Vec<PointRow>>(WRITER_CHAN_CAP);
            let h = match cfg.output {
                Output::Pg => tokio::spawn(copy_writer::<PointRow>(pool.clone().unwrap(), point_table.clone(), POINT_COLUMNS, rx)),
                Output::Csv | Output::GeoJson => tokio::spawn(csv_writer::<PointRow>(out_dir.join(format!("{point_table}.csv")), POINT_COLUMNS, rx)),
            };
            th.push(h);
            ts.push(tx);
        }
        point_senders.push(ts);
        point_handles.push(th);
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
    let way_linestring_topics = Arc::new(way_linestring_topics);
    let way_polygon_topics = Arc::new(way_polygon_topics);
    let point_topics = Arc::new(point_topics);
    let way_point_eligible = Arc::new(way_point_eligible);
    let node_point_eligible = Arc::new(node_point_eligible);
    // Shared, thread-safe state captured by the reader's callbacks (called from rayon workers).
    let runners = Arc::new(runners);
    let tag_senders = Arc::new(tag_senders);
    let geom_senders = Arc::new(geom_senders);
    let member_senders = Arc::new(member_senders);
    let way_geom_senders = Arc::new(way_geom_senders);
    let polygon_senders = Arc::new(polygon_senders);
    let point_senders = Arc::new(point_senders);
    let node_senders = Arc::new(node_senders);
    let tag_rr: Arc<Vec<AtomicUsize>> = Arc::new((0..n).map(|_| AtomicUsize::new(0)).collect());
    let geom_rr = Arc::new(AtomicUsize::new(0));
    let member_rr = Arc::new(AtomicUsize::new(0));
    let node_rr = Arc::new(AtomicUsize::new(0));
    let way_geom_rr: Arc<Vec<AtomicUsize>> =
        Arc::new((0..way_geom_senders.len()).map(|_| AtomicUsize::new(0)).collect());
    let polygon_rr: Arc<Vec<AtomicUsize>> =
        Arc::new((0..polygon_senders.len()).map(|_| AtomicUsize::new(0)).collect());
    let point_rr: Arc<Vec<AtomicUsize>> =
        Arc::new((0..point_senders.len()).map(|_| AtomicUsize::new(0)).collect());
    // Every kept relation wanting geometry, recorded independently by `classify_rel_cb` — `(rel_id,
    // member ways with role, per-topic keep mask)`. Consumed by the post-stream relation-geometry
    // step below, alongside `rel_way_coords` (see next).
    let rel_geom_requests: Arc<Mutex<Vec<(i64, Vec<(i64, osm::types::MemberRole)>, u32)>>> = Arc::new(Mutex::new(Vec::new()));
    // The reader's side channel (see `Callbacks::extra_way_ids`'s own doc): `classify_rel_cb` below
    // extends this with every geometry-wanting relation's member way ids — the *same* ids as
    // `rel_geom_requests`, just flattened — so the reader's own Pass A/B decode resolves their
    // coordinates as it goes, no second PBF scan needed for relation geometry.
    let extra_way_ids: Arc<Mutex<FxHashSet<i64>>> = Arc::new(Mutex::new(FxHashSet::default()));
    // Filled once, by `build_extra_geom_cb`, after the reader's Pass B/Scan 2 resolves coordinates.
    let rel_way_coords: Arc<Mutex<FxHashMap<i64, Vec<(f64, f64)>>>> = Arc::new(Mutex::new(FxHashMap::default()));
    // `runners`/`rel_geom_requests`/`rel_way_coords` are still needed after the producer for
    // post-processing (graph materialization, relation geometry), so the producer closure gets its
    // own clones rather than moving the originals.
    let producer_runners = runners.clone();
    let rel_geom_requests_outer = rel_geom_requests.clone();
    let rel_way_coords_outer = rel_way_coords.clone();
    let producer = tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        let runners = producer_runners;
        // Ways pass: emit tag rows; a way is kept purely by its own tag classification — relation
        // membership has no bearing here (see `Callbacks::classify_way`'s own doc). Payload is the
        // per-topic keep bitmask (see `ClassifyOutput`), used by `build_geom_cb` to route
        // way-shaped rows to just the topics that both kept the way and want one.
        let classify_way_cb = {
            let (runners, tag_senders, tag_rr) = (runners.clone(), tag_senders.clone(), tag_rr.clone());
            move |wd: &WayData| -> Option<u32> {
                let out = classify_way(&runners, wd);
                let kept_by_topic = route_tag_rows(out.topic_rows, &tag_senders, &tag_rr, w);
                kept_by_topic.then_some(out.mask)
            }
        };
        // Relations pass: emit relation tag rows + `relation_members` links; kept iff some topic
        // categorized the relation. Fully independent of the ways/geometry passes for
        // classification purposes — a kept relation wanting geometry (line/point/polygon) records
        // its member way ids into both `rel_geom_requests` (own bookkeeping) and `extra_way_ids`
        // (the reader's side channel, see `Callbacks::extra_way_ids`), so the reader's Pass A/B
        // resolves their coordinates as part of its normal decode, no second scan needed.
        let classify_rel_cb = {
            let (runners, tag_senders, tag_rr) = (runners.clone(), tag_senders.clone(), tag_rr.clone());
            let (member_senders, member_rr) = (member_senders.clone(), member_rr.clone());
            let rel_geom_requests = rel_geom_requests.clone();
            let extra_way_ids = extra_way_ids.clone();
            move |rd: &RelData| -> bool {
                let rows = classify_relation(&runners, rd);
                let mut mask = 0u32;
                for (i, r) in rows.iter().enumerate() {
                    if !r.is_empty() {
                        mask |= 1 << i;
                    }
                }
                let kept = route_tag_rows(rows, &tag_senders, &tag_rr, w);
                if kept && !rd.member_ways.is_empty() {
                    let links: Vec<MemberRow> = rd
                        .member_ways
                        .iter()
                        .map(|&(wid, _)| MemberRow { relation_osm_id: rd.id, way_osm_id: wid })
                        .collect();
                    let kk = member_rr.fetch_add(1, Ordering::Relaxed) % w;
                    let _ = member_senders[kk].blocking_send(links);

                    let wants_geom = mask & rel_geom_mask != 0;
                    if wants_geom {
                        extra_way_ids.lock().unwrap().extend(rd.member_ways.iter().map(|&(wid, _)| wid));
                        rel_geom_requests.lock().unwrap().push((rd.id, rd.member_ways.clone(), mask));
                    }
                }
                kept
            }
        };
        // Nodes pass: emit node tag rows; a node is "selected" (forced cut point) iff some topic
        // categorized it. Also routes this node's own point row to every topic that both kept it
        // and declared `"geometry": { "node": ["point"] }`.
        let classify_node_cb = {
            let (runners, tag_senders, tag_rr) = (runners.clone(), tag_senders.clone(), tag_rr.clone());
            let (point_senders, point_rr) = (point_senders.clone(), point_rr.clone());
            let (point_topics, node_point_eligible) = (point_topics.clone(), node_point_eligible.clone());
            move |nd: &NodeData| -> bool {
                let rows = classify_node(&runners, nd);
                let mut mask = 0u32;
                for (i, r) in rows.iter().enumerate() {
                    if !r.is_empty() {
                        mask |= 1 << i;
                    }
                }
                let kept = route_tag_rows(rows, &tag_senders, &tag_rr, w);
                if kept && !point_topics.is_empty() {
                    let row = build_node_point_row(nd.id, nd.lon, nd.lat);
                    route_point_row(&row, mask, &point_topics, &node_point_eligible, &point_senders, &point_rr, w);
                }
                kept
            }
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
        // Geometry pass: write the resolved way's graph edges to the shared table, plus its
        // whole-way linestring/polygon/centroid-point to every topic that both kept this way
        // (`mask`) and declared the matching shape.
        let build_geom_cb = {
            let (geom_senders, geom_rr) = (geom_senders.clone(), geom_rr.clone());
            let way_geom_senders = way_geom_senders.clone();
            let (way_geom_rr, way_linestring_topics) = (way_geom_rr.clone(), way_linestring_topics.clone());
            let (polygon_senders, polygon_rr, way_polygon_topics) =
                (polygon_senders.clone(), polygon_rr.clone(), way_polygon_topics.clone());
            let (point_senders, point_rr) = (point_senders.clone(), point_rr.clone());
            let (point_topics, way_point_eligible) = (point_topics.clone(), way_point_eligible.clone());
            move |way: &OsmWay, mask: u32, node_ids: &FxHashMap<i64, i64>| {
                let kk = geom_rr.fetch_add(1, Ordering::Relaxed) % w;
                let _ = geom_senders[kk].blocking_send(geom_rows_for(way, node_ids));
                if !way_linestring_topics.is_empty() || !point_topics.is_empty() {
                    let geom = geom::primitives::project_line(&way.coords);
                    if !way_linestring_topics.is_empty() {
                        let row = way_geom_row_for(way);
                        route_shape_row(&row, mask, &way_linestring_topics, &way_geom_senders, &way_geom_rr, w);
                    }
                    if !point_topics.is_empty() {
                        if let Some(row) = build_way_point_row(way, &geom) {
                            route_point_row(&row, mask, &point_topics, &way_point_eligible, &point_senders, &point_rr, w);
                        }
                    }
                }
                if !way_polygon_topics.is_empty() {
                    let row = build_way_polygon_row(way);
                    route_shape_row(&row, mask, &way_polygon_topics, &polygon_senders, &polygon_rr, w);
                }
            }
        };
        // Relation-geometry coordinate resolution: called once, after Pass B/Scan 2, with every
        // `extra_way_ids` member way's resolved coordinate sequence — stashed for the post-stream
        // relation-geometry assembly step below.
        let build_extra_geom_cb = {
            let rel_way_coords = rel_way_coords.clone();
            move |resolved: FxHashMap<i64, Vec<(f64, f64)>>| {
                *rel_way_coords.lock().unwrap() = resolved;
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
                extra_way_ids,
                build_extra_geom: build_extra_geom_cb,
            },
        )
    });

    // Await the producer first: it drops the senders, closing every writer channel so the writer
    // tasks drain their tails, finish the COPY, and return their counts.
    producer.await.context("reader/processing task panicked")??;

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
    let mut way_geom_counts = vec![0usize; way_geom_handles.len()];
    for (i, handles) in way_geom_handles.into_iter().enumerate() {
        for h in handles {
            way_geom_counts[i] += h.await.context("way-geom writer panicked")??;
        }
    }
    let mut node_count = 0usize;
    for h in node_handles {
        node_count += h.await.context("node writer panicked")??;
    }
    let mut polygon_counts = vec![0usize; polygon_handles.len()];
    for (i, handles) in polygon_handles.into_iter().enumerate() {
        for h in handles {
            polygon_counts[i] += h.await.context("polygon writer panicked")??;
        }
    }
    let mut point_counts = vec![0usize; point_handles.len()];
    for (i, handles) in point_handles.into_iter().enumerate() {
        for h in handles {
            point_counts[i] += h.await.context("point writer panicked")??;
        }
    }

    for (i, table) in tables.iter().enumerate() {
        info!("Wrote {} tag rows → {}", tag_counts[i], table);
    }
    info!("Wrote {geom_count} edge rows → {EDGE_TABLE}");
    info!("Wrote {member_count} relation-member links → {MEMBER_TABLE}");
    info!("Wrote {node_count} node rows → {NODE_TABLE}");
    for (i, &table_name) in way_geom_table_refs.iter().enumerate() {
        info!("Wrote {} way rows → {}_geom", way_geom_counts[i], table_name);
    }
    for (i, &table_name) in polygon_table_refs.iter().enumerate() {
        info!("Wrote {} rows → {}_polygon", polygon_counts[i], table_name);
    }
    for (i, &table_name) in point_table_refs.iter().enumerate() {
        info!("Wrote {} rows → {}_point", point_counts[i], table_name);
    }
    info!("Read + process time: {:.1}s", t0.elapsed().as_secs_f32());
    osmnexus::profiling::report();

    // Relation geometry (line/point/polygon): `classify_rel_cb` recorded which kept relations want
    // geometry plus their raw member way ids, both into `rel_geom_requests` and into the reader's
    // `extra_way_ids` side channel — the reader's own Pass A/B decode resolved those member ways'
    // coordinates as it went (no second PBF scan; see `Callbacks::extra_way_ids`'s own doc), handed
    // back here via `build_extra_geom_cb` into `rel_way_coords`. This still works the same for
    // CSV/GeoJSON as for Postgres (unlike the old SQL-post-processing approach, which needed a live
    // database to merge from).
    let rel_geom_requests: Vec<(i64, Vec<(i64, osm::types::MemberRole)>, u32)> =
        std::mem::take(&mut *rel_geom_requests_outer.lock().unwrap());
    if !rel_geom_requests.is_empty() {
        info!("Resolving relation geometry ({} relations)...", rel_geom_requests.len());
        let t_rel = std::time::Instant::now();
        let way_coords = std::mem::take(&mut *rel_way_coords_outer.lock().unwrap());

        let mut line_rows: Vec<Vec<WayGeomRow>> = vec![Vec::new(); relation_line_topics.len()];
        let mut point_rows: Vec<Vec<PointRow>> = vec![Vec::new(); relation_point_topics.len()];
        let mut polygon_rows: Vec<Vec<PolygonRow>> = vec![Vec::new(); relation_polygon_topics.len()];
        for (rel_id, members, mask) in &rel_geom_requests {
            let member_coords: Vec<Vec<(f64, f64)>> =
                members.iter().filter_map(|&(w, _)| way_coords.get(&w).cloned()).collect();
            if let Some(row) = geom::builders::build_relation_line_row(*rel_id, &member_coords) {
                for (i, &topic_idx) in relation_line_topics.iter().enumerate() {
                    if mask & (1 << topic_idx) != 0 {
                        line_rows[i].push(row.clone());
                    }
                }
            }
            if let Some(row) = geom::builders::build_relation_point_row(*rel_id, &member_coords) {
                for (i, &topic_idx) in relation_point_topics.iter().enumerate() {
                    if mask & (1 << topic_idx) != 0 {
                        point_rows[i].push(row.clone());
                    }
                }
            }
            if !relation_polygon_topics.is_empty() {
                use osm::types::MemberRole;
                let outer_coords: Vec<Vec<(f64, f64)>> = members
                    .iter()
                    .filter(|&&(_, role)| role != MemberRole::Inner)
                    .filter_map(|&(w, _)| way_coords.get(&w).cloned())
                    .collect();
                let inner_coords: Vec<Vec<(f64, f64)>> = members
                    .iter()
                    .filter(|&&(_, role)| role == MemberRole::Inner)
                    .filter_map(|&(w, _)| way_coords.get(&w).cloned())
                    .collect();
                let outer_rings = geom::relation::assemble_rings(outer_coords);
                let inner_rings = geom::relation::assemble_rings(inner_coords);
                if let Some(row) = geom::builders::build_relation_polygon_row(*rel_id, &outer_rings, &inner_rings) {
                    for (i, &topic_idx) in relation_polygon_topics.iter().enumerate() {
                        if mask & (1 << topic_idx) != 0 {
                            polygon_rows[i].push(row.clone());
                        }
                    }
                }
            }
        }
        for (i, &table_name) in relation_line_table_refs.iter().enumerate() {
            let out_table = schema::relation_geom_table(table_name);
            let rows = std::mem::take(&mut line_rows[i]);
            let count = write_rows_once(cfg.output, &pool, &out_dir, &out_table, WAY_GEOM_COLUMNS, rows).await?;
            info!("Wrote {count} rows → {out_table}");
        }
        for (i, &table_name) in relation_point_table_refs.iter().enumerate() {
            let out_table = schema::relation_point_table(table_name);
            let rows = std::mem::take(&mut point_rows[i]);
            let count = write_rows_once(cfg.output, &pool, &out_dir, &out_table, POINT_COLUMNS, rows).await?;
            info!("Wrote {count} rows → {out_table}");
        }
        for (i, &table_name) in relation_polygon_table_refs.iter().enumerate() {
            let out_table = schema::relation_polygon_table(table_name);
            let rows = std::mem::take(&mut polygon_rows[i]);
            let count = write_rows_once(cfg.output, &pool, &out_dir, &out_table, POLYGON_COLUMNS, rows).await?;
            info!("Wrote {count} rows → {out_table}");
        }
        info!("Relation geometry resolution: {:.1}s", t_rel.elapsed().as_secs_f32());
    }

    match (cfg.output, cfg.create_index) {
        (Output::Pg, true) => {
            info!("Creating indexes...");
            let t_idx = std::time::Instant::now();
            schema::create_indexes(pool.as_ref().unwrap(), &table_refs, &geom_tables).await?;
            info!("Index creation: {:.1}s", t_idx.elapsed().as_secs_f32());
        }
        (Output::Pg, false) => info!("Skipping index creation (pass --create-index to enable)"),
        (Output::Csv, _) | (Output::GeoJson, _) => {}
    }

    if cfg.output == Output::Pg {
        let client = pool.as_ref().unwrap().get().await?;
        for r in runners.iter().filter(|r| r.wants_way_graph()) {
            info!("Materializing graph edges → {}_edge", r.table());
            db::topic_edges::materialize(&client, r.table(), cfg.topic_edges, cfg.create_index).await?;
        }
    }

    if cfg.output == Output::GeoJson {
        info!("Building GeoJSON from CSV output...");
        output::geojson::write_geojson_from_csv(&out_dir, &tables)?;
        for table in &tables {
            info!("Wrote {}/{table}.geojson", cfg.out_dir);
        }
    }

    info!("Done. Total: {:.1}s", t0.elapsed().as_secs_f32());
    Ok(())
}
