use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use osmnexus::{config, db, geom, osm, output, processing, topic};

use anyhow::Context;
use clap::Parser;
use deadpool_postgres::Pool;
use tokio::sync::mpsc;
use tracing::info;

use config::{Config, Output};
use db::{
    pool::build_pool,
    schema,
    schema::{EDGE_TABLE, MEMBER_TABLE, NODE_TABLE},
};
use geom::rows::{
    EdgeRow, NodeRow, PointRow, PolygonRow, WayRow, EDGE_COLUMNS, NODE_COLUMNS, POINT_COLUMNS,
    POLYGON_COLUMNS, WAY_COLUMNS,
};
use topic::TopicRunner;
use osm::types::{ElementKind, NodeData, RelData, WayData};
use output::rows::{CsvRow, MemberRow, TopicRow, MEMBER_COLUMNS, TAG_COLUMNS};
use output::writers::{copy_writer, csv_writer};
use processing::{classify_node, classify_relation, classify_way};

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
    // coords are collected regardless of `has_nodes` (needed for way geometry resolution
    // regardless), but the graph-vertex `nodes` table itself is now gated separately by
    // `plan.any_way_graph` below (see `schema::EDGE_TABLE`'s own doc).
    let has_relations = runners.iter().any(|r| r.has_kind(ElementKind::Relation));
    let has_nodes = runners.iter().any(|r| r.has_kind(ElementKind::Node));

    // Every geometry decision (which topic wants which shape, for which kind) computed once —
    // replaces what used to be a dozen separately-named `Vec<usize>`/`Vec<bool>` locals here. See
    // `geom::plan::GeometryPlan`'s own doc.
    let plan = geom::plan::GeometryPlan::build(&runners);

    // Every non-tag-table geometry table this run needs (see `schema::GeomTableShape`'s own doc) —
    // one consolidated list instead of a parallel `&[&str]` per shape.
    let geom_tables: Vec<(String, schema::GeomTableShape)> = plan
        .way_line_topics
        .iter()
        .map(|&i| (schema::way_geom_table(table_refs[i]), schema::GeomTableShape::LineString))
        .chain(plan.way_polygon_topics.iter().map(|&i| (schema::polygon_table(table_refs[i]), schema::GeomTableShape::Polygon)))
        .chain(plan.point_topics.iter().map(|&i| (schema::point_table(table_refs[i]), schema::GeomTableShape::Point)))
        .chain(plan.relation_line_topics.iter().map(|&i| (schema::relation_geom_table(table_refs[i]), schema::GeomTableShape::LineString)))
        .chain(plan.relation_point_topics.iter().map(|&i| (schema::relation_point_table(table_refs[i]), schema::GeomTableShape::Point)))
        .chain(plan.relation_polygon_topics.iter().map(|&i| (schema::relation_polygon_table(table_refs[i]), schema::GeomTableShape::Polygon)))
        .collect();
    let way_geom_table_refs: Vec<&str> = plan.way_line_topics.iter().map(|&i| table_refs[i]).collect();
    let polygon_table_refs: Vec<&str> = plan.way_polygon_topics.iter().map(|&i| table_refs[i]).collect();
    let point_table_refs: Vec<&str> = plan.point_topics.iter().map(|&i| table_refs[i]).collect();
    let relation_line_table_refs: Vec<&str> = plan.relation_line_topics.iter().map(|&i| table_refs[i]).collect();
    let relation_point_table_refs: Vec<&str> = plan.relation_point_topics.iter().map(|&i| table_refs[i]).collect();
    let relation_polygon_table_refs: Vec<&str> = plan.relation_polygon_topics.iter().map(|&i| table_refs[i]).collect();

    let n = tables.len();
    // Extra sharded-writer tables beyond the per-topic tag tables: members, plus edges+nodes when
    // any topic wants the graph, plus one per topic wanting a way-shaped geometry table (relation
    // geometry writes separately, after the main streaming pass, so it isn't part of this
    // pool-sizing count — see below).
    let extra_tables = 1
        + if plan.any_way_graph { 2 } else { 0 }
        + plan.way_line_topics.len()
        + plan.way_polygon_topics.len()
        + plan.point_topics.len();

    // Output backend. `w` = parallel writers per table: k sharded COPY connections for Postgres, a
    // single file writer for CSV. `pool` is `None` for CSV.
    let (pool, w): (Option<Pool>, usize) = match cfg.output {
        Output::Pg => {
            info!("Connecting to database {}@{}/{}", cfg.db_user, cfg.db_host, cfg.db_name);
            let pool = build_pool(&cfg)?;
            let client_setup = pool.get().await.context("getting DB connection")?;
            info!("Setting up schema...");
            schema::create_tables(&client_setup, &table_refs, &geom_tables, plan.any_way_graph).await?;
            if cfg.truncate {
                schema::truncate_tables(&client_setup, &table_refs, &geom_tables, plan.any_way_graph).await?;
            }
            schema::drop_indexes(&client_setup, &table_refs, &geom_tables, plan.any_way_graph).await?;
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
    // Shared graph tables (`edges`/`nodes`) — only spawned when some topic actually wants the
    // routing graph (see `plan.any_way_graph`'s own doc); otherwise both stay empty, so no table/file
    // is created at all and the reader skips the corresponding work (see `build_geom_cb`/
    // `build_nodes_cb` below).
    let mut geom_senders: Vec<mpsc::Sender<Vec<EdgeRow>>> = Vec::with_capacity(if plan.any_way_graph { w } else { 0 });
    let mut geom_handles: Vec<tokio::task::JoinHandle<anyhow::Result<usize>>> = Vec::with_capacity(if plan.any_way_graph { w } else { 0 });
    if plan.any_way_graph {
        for _ in 0..w {
            let (tx, rx) = mpsc::channel::<Vec<EdgeRow>>(WRITER_CHAN_CAP);
            let h = match cfg.output {
                Output::Pg => tokio::spawn(copy_writer::<EdgeRow>(pool.clone().unwrap(), EDGE_TABLE.to_owned(), EDGE_COLUMNS, rx)),
                Output::Csv | Output::GeoJson => tokio::spawn(csv_writer::<EdgeRow>(out_dir.join(format!("{EDGE_TABLE}.csv")), EDGE_COLUMNS, rx)),
            };
            geom_handles.push(h);
            geom_senders.push(tx);
        }
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
    let mut way_geom_senders: Vec<Vec<mpsc::Sender<Vec<WayRow>>>> = Vec::with_capacity(way_geom_table_refs.len());
    let mut way_geom_handles: Vec<Vec<tokio::task::JoinHandle<anyhow::Result<usize>>>> = Vec::with_capacity(way_geom_table_refs.len());
    for &table_name in &way_geom_table_refs {
        let way_geom_table = schema::way_geom_table(table_name);
        let (mut ts, mut th) = (Vec::with_capacity(w), Vec::with_capacity(w));
        for _ in 0..w {
            let (tx, rx) = mpsc::channel::<Vec<WayRow>>(WRITER_CHAN_CAP);
            let h = match cfg.output {
                Output::Pg => tokio::spawn(copy_writer::<WayRow>(pool.clone().unwrap(), way_geom_table.clone(), WAY_COLUMNS, rx)),
                Output::Csv | Output::GeoJson => tokio::spawn(csv_writer::<WayRow>(out_dir.join(format!("{way_geom_table}.csv")), WAY_COLUMNS, rx)),
            };
            th.push(h);
            ts.push(tx);
        }
        way_geom_senders.push(ts);
        way_geom_handles.push(th);
    }
    // One polygon table per topic declaring `"geometry": { "way": ["polygon"] }`.
    let mut polygon_senders: Vec<Vec<mpsc::Sender<Vec<PolygonRow>>>> = Vec::with_capacity(polygon_table_refs.len());
    let mut polygon_handles: Vec<Vec<tokio::task::JoinHandle<anyhow::Result<usize>>>> = Vec::with_capacity(polygon_table_refs.len());
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
    let mut point_senders: Vec<Vec<mpsc::Sender<Vec<PointRow>>>> = Vec::with_capacity(point_table_refs.len());
    let mut point_handles: Vec<Vec<tokio::task::JoinHandle<anyhow::Result<usize>>>> = Vec::with_capacity(point_table_refs.len());
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
    // Graph-vertex table — same `plan.any_way_graph` gate as `edges` (see above); the two always exist
    // or don't together.
    let mut node_senders: Vec<mpsc::Sender<Vec<NodeRow>>> = Vec::with_capacity(if plan.any_way_graph { w } else { 0 });
    let mut node_handles: Vec<tokio::task::JoinHandle<anyhow::Result<usize>>> = Vec::with_capacity(if plan.any_way_graph { w } else { 0 });
    if plan.any_way_graph {
        for _ in 0..w {
            let (tx, rx) = mpsc::channel::<Vec<NodeRow>>(WRITER_CHAN_CAP);
            let h = match cfg.output {
                Output::Pg => tokio::spawn(copy_writer::<NodeRow>(pool.clone().unwrap(), NODE_TABLE.to_owned(), NODE_COLUMNS, rx)),
                Output::Csv | Output::GeoJson => tokio::spawn(csv_writer::<NodeRow>(out_dir.join(format!("{NODE_TABLE}.csv")), NODE_COLUMNS, rx)),
            };
            node_handles.push(h);
            node_senders.push(tx);
        }
    }

    // Select phase: the reader decodes the PBF once and drives the classify callbacks below,
    // streaming tag rows out as it goes (side effect — see `osm::reader`'s own doc for why that
    // can't wait), and returns a `SelectionContext` once finished. Runs on the blocking pool
    // (CPU-bound rayon work).
    let pbf_file = cfg.pbf_file.clone();
    let plan = Arc::new(plan);
    // Shared, thread-safe state captured by the reader's callbacks (called from rayon workers).
    let runners = Arc::new(runners);
    let tag_senders = Arc::new(tag_senders);
    let member_senders = Arc::new(member_senders);
    let point_senders = Arc::new(point_senders);
    let tag_rr: Arc<Vec<AtomicUsize>> = Arc::new((0..n).map(|_| AtomicUsize::new(0)).collect());
    let member_rr = Arc::new(AtomicUsize::new(0));
    let point_rr: Arc<Vec<AtomicUsize>> = Arc::new((0..point_senders.len()).map(|_| AtomicUsize::new(0)).collect());
    let producer_runners = runners.clone();
    let producer_plan = plan.clone();
    // Needed again in the materialize phase below (way points share the same point channels/
    // counters node points already used), so keep our own clones rather than letting the select
    // closure consume the only reference.
    let point_senders_outer = point_senders.clone();
    let point_rr_outer = point_rr.clone();
    let select_task = tokio::task::spawn_blocking(move || -> anyhow::Result<osm::reader::SelectionContext> {
        let runners = producer_runners;
        let plan = producer_plan;
        // Ways pass: emit tag rows; a way is kept purely by its own tag classification — relation
        // membership has no bearing here (see `osm::reader::Callbacks::classify_way`'s own doc).
        // The returned mask becomes `SelectionContext::way_refs`' per-way keep mask.
        let classify_way_cb = {
            let (runners, tag_senders, tag_rr) = (runners.clone(), tag_senders.clone(), tag_rr.clone());
            move |wd: &WayData| -> Option<u32> {
                let out = classify_way(&runners, wd);
                let kept_by_topic = route_tag_rows(out.topic_rows, &tag_senders, &tag_rr, w);
                kept_by_topic.then_some(out.mask)
            }
        };
        // Relations pass: emit relation tag rows + `relation_members` links; return the keep mask.
        // Fully independent of the ways pass for classification purposes — a kept relation's
        // member ways are recorded into `SelectionContext::rel_members` by the reader itself
        // (regardless of whether any topic wants relation *geometry*; that decision is
        // `geom::materialize`'s, using `plan`, not made here).
        let classify_rel_cb = {
            let (runners, tag_senders, tag_rr) = (runners.clone(), tag_senders.clone(), tag_rr.clone());
            let (member_senders, member_rr) = (member_senders.clone(), member_rr.clone());
            move |rd: &RelData| -> Option<u32> {
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
                }
                kept.then_some(mask)
            }
        };
        // Nodes pass: emit node tag rows; a node is "selected" (forced cut point) iff some topic
        // categorized it. Also builds + routes this node's own point row right here — a node is a
        // leaf, its point shape needs nothing `SelectionContext` provides, so there's no reason to
        // defer it to the materialize phase the way way/relation geometry is deferred.
        let classify_node_cb = {
            let (runners, tag_senders, tag_rr) = (runners.clone(), tag_senders.clone(), tag_rr.clone());
            let (point_senders, point_rr) = (point_senders.clone(), point_rr.clone());
            let plan = plan.clone();
            move |nd: &NodeData| -> bool {
                let rows = classify_node(&runners, nd);
                let mut mask = 0u32;
                for (i, r) in rows.iter().enumerate() {
                    if !r.is_empty() {
                        mask |= 1 << i;
                    }
                }
                let kept = route_tag_rows(rows, &tag_senders, &tag_rr, w);
                if kept {
                    if let Some(row) = geom::materialize::node_point(nd.id, nd.lon, nd.lat, &plan) {
                        route_point_row(&row, mask, &plan.point_topics, &plan.node_point_eligible, &point_senders, &point_rr, w);
                    }
                }
                kept
            }
        };
        osm::reader::stream_osm(
            &pbf_file,
            osm::reader::Callbacks {
                has_relations,
                classify_rel: classify_rel_cb,
                classify_way: classify_way_cb,
                has_nodes,
                classify_node: classify_node_cb,
            },
        )
    });

    // Await the select phase: it drops the tag/member/point senders, closing those writer channels
    // so their writer tasks drain their tails, finish the COPY, and return their counts. The
    // geometry-table writers (edges/nodes/way-line/polygon) are still open — they're only fed
    // during the materialize phase below.
    let ctx = select_task.await.context("select-phase task panicked")??;

    let mut tag_counts = vec![0usize; n];
    for (i, handles) in tag_handles.into_iter().enumerate() {
        for h in handles {
            tag_counts[i] += h.await.context("tag writer panicked")??;
        }
    }
    let mut member_count = 0usize;
    for h in member_handles {
        member_count += h.await.context("member writer panicked")??;
    }
    let mut point_counts = vec![0usize; point_handles.len()];
    // Point-table writers only get node-point rows so far (way points come from the materialize
    // phase below, into the *same* channels) — don't await/close them yet.

    for (i, table) in tables.iter().enumerate() {
        info!("Wrote {} tag rows → {}", tag_counts[i], table);
    }
    info!("Wrote {member_count} relation-member links → {MEMBER_TABLE}");
    info!("Select phase time: {:.1}s", t0.elapsed().as_secs_f32());

    // Materialize phase: resolve way/relation geometry from `ctx` and route every row to its
    // writer channel — see `geom::materialize::run`'s own doc. Runs on the blocking pool (rayon
    // work inside); routing itself is sequential (one thread), unlike the old design where it was
    // naturally parallelized by running per-way inside the reader's own parallel geometry pass —
    // a possible future optimization if routing throughput ever becomes the bottleneck.
    let t_mat = std::time::Instant::now();
    let materialize_plan = plan.clone();
    // Round-robin counters for this phase's routing. `way_geom_rr`/`polygon_rr` are per-table (one
    // counter per topic wanting that shape, matching `route_shape_row`'s expectations); `geom_rr`/
    // `node_rr` are single counters for the one shared edges/nodes table. Plain (not `Arc`) since
    // routing here is sequential, unlike the select phase's genuinely concurrent rayon workers —
    // `point_rr`/`point_senders` are the one exception, still the same `Arc` the select phase used,
    // continuing its round-robin rather than resetting it.
    let way_geom_rr: Vec<AtomicUsize> = (0..way_geom_senders.len()).map(|_| AtomicUsize::new(0)).collect();
    let polygon_rr: Vec<AtomicUsize> = (0..polygon_senders.len()).map(|_| AtomicUsize::new(0)).collect();
    let point_senders_mat = point_senders_outer.clone();
    let point_rr_mat = point_rr_outer.clone();
    let relations_batch = tokio::task::spawn_blocking(move || {
        let (point_senders, point_rr) = (point_senders_mat, point_rr_mat);
        let (mut geom_rr, mut node_rr) = (0usize, 0usize);
        let m = geom::materialize::run(&ctx, &materialize_plan);

        if !node_senders.is_empty() {
            let mut rows = m.node_rows;
            while !rows.is_empty() {
                let take = rows.len().min(4096);
                let chunk: Vec<NodeRow> = rows.drain(..take).collect();
                let kk = node_rr % w;
                node_rr += 1;
                let _ = node_senders[kk].blocking_send(chunk);
            }
        }
        for (_way_id, mask, g) in m.ways {
            if let Some(rows) = g.edges {
                let kk = geom_rr % w;
                geom_rr += 1;
                let _ = geom_senders[kk].blocking_send(rows);
            }
            if let Some(row) = g.line {
                route_shape_row(&row, mask, &materialize_plan.way_line_topics, &way_geom_senders, &way_geom_rr, w);
            }
            if let Some(row) = g.point {
                route_point_row(&row, mask, &materialize_plan.point_topics, &materialize_plan.way_point_eligible, &point_senders, &point_rr, w);
            }
            if let Some(row) = g.polygon {
                route_shape_row(&row, mask, &materialize_plan.way_polygon_topics, &polygon_senders, &polygon_rr, w);
            }
        }
        m.relations
    })
    .await
    .context("materialize-phase task panicked")?;
    info!("Materialize phase time: {:.1}s", t_mat.elapsed().as_secs_f32());
    // The select phase's clone of `point_senders` already dropped when `select_task` finished;
    // this was the only other clone, so dropping it now closes the point-table channels, letting
    // the point writers below finish draining.
    drop(point_senders_outer);

    let mut geom_count = 0usize;
    for h in geom_handles {
        geom_count += h.await.context("geom writer panicked")??;
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
    for (i, handles) in point_handles.into_iter().enumerate() {
        for h in handles {
            point_counts[i] += h.await.context("point writer panicked")??;
        }
    }

    if plan.any_way_graph {
        info!("Wrote {geom_count} edge rows → {EDGE_TABLE}");
        info!("Wrote {node_count} node rows → {NODE_TABLE}");
    }
    for (i, &table_name) in way_geom_table_refs.iter().enumerate() {
        info!("Wrote {} way rows → {}_geom", way_geom_counts[i], table_name);
    }
    for (i, &table_name) in polygon_table_refs.iter().enumerate() {
        info!("Wrote {} rows → {}_polygon", polygon_counts[i], table_name);
    }
    for (i, &table_name) in point_table_refs.iter().enumerate() {
        info!("Wrote {} rows → {}_point", point_counts[i], table_name);
    }
    osmnexus::profiling::report();
    for r in runners.iter() {
        r.field_stages.report();
    }

    // Relation geometry (line/point/polygon): already built by the materialize phase above, from
    // `ctx.rel_members` — just write it out. Works the same for CSV/GeoJSON as for Postgres
    // (unlike the old SQL-post-processing approach, which needed a live database to merge from).
    let mut batch = relations_batch;
    for (i, &table_name) in relation_line_table_refs.iter().enumerate() {
        let out_table = schema::relation_geom_table(table_name);
        let rows = std::mem::take(&mut batch.line_rows[i]);
        let count = write_rows_once(cfg.output, &pool, &out_dir, &out_table, WAY_COLUMNS, rows).await?;
        info!("Wrote {count} rows → {out_table}");
    }
    for (i, &table_name) in relation_point_table_refs.iter().enumerate() {
        let out_table = schema::relation_point_table(table_name);
        let rows = std::mem::take(&mut batch.point_rows[i]);
        let count = write_rows_once(cfg.output, &pool, &out_dir, &out_table, POINT_COLUMNS, rows).await?;
        info!("Wrote {count} rows → {out_table}");
    }
    for (i, &table_name) in relation_polygon_table_refs.iter().enumerate() {
        let out_table = schema::relation_polygon_table(table_name);
        let rows = std::mem::take(&mut batch.polygon_rows[i]);
        let count = write_rows_once(cfg.output, &pool, &out_dir, &out_table, POLYGON_COLUMNS, rows).await?;
        info!("Wrote {count} rows → {out_table}");
    }

    match (cfg.output, cfg.create_index) {
        (Output::Pg, true) => {
            info!("Creating indexes...");
            let t_idx = std::time::Instant::now();
            schema::create_indexes(pool.as_ref().unwrap(), &table_refs, &geom_tables, plan.any_way_graph).await?;
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
