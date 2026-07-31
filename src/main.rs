use std::path::PathBuf;
use std::sync::Arc;

#[cfg(feature = "dhat")]
#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

/// Print a heap snapshot labeled `phase`: current live bytes/blocks, plus the delta in
/// cumulative total allocated since the *previous* call (dhat's own totals only reset at process
/// start, so this tracks the last-seen totals itself to report per-phase deltas). No-op without
/// the `dhat` feature.
#[cfg(feature = "dhat")]
fn mem_snapshot(phase: &str) {
    use std::sync::atomic::{AtomicU64, Ordering};
    static PREV_BLOCKS: AtomicU64 = AtomicU64::new(0);
    static PREV_BYTES: AtomicU64 = AtomicU64::new(0);
    let s = dhat::HeapStats::get();
    let prev_blocks = PREV_BLOCKS.swap(s.total_blocks as u64, Ordering::Relaxed);
    let prev_bytes = PREV_BYTES.swap(s.total_bytes as u64, Ordering::Relaxed);
    tracing::info!(
        "[mem] {phase}: curr_blocks={} curr_bytes={} total_blocks Δ={} total_bytes Δ={} max_blocks={} max_bytes={}",
        s.curr_blocks,
        s.curr_bytes,
        s.total_blocks as u64 - prev_blocks,
        s.total_bytes as u64 - prev_bytes,
        s.max_blocks,
        s.max_bytes,
    );
}
#[cfg(not(feature = "dhat"))]
fn mem_snapshot(_phase: &str) {}

use osmnexus::{config, db, geom, osm, output, processing, topic};

use anyhow::Context;
use clap::Parser;
use deadpool_postgres::Pool;
use tracing::info;

use config::{Config, Output};
use db::schema;
use geom::rows::{POINT_COLUMNS, POLYGON_COLUMNS, WAY_COLUMNS};
use topic::TopicRunner;
use osm::types::{ElementKind, NodeData, RelData, WayData};
use output::rows::MemberRow;
use output::sinks::{write_rows_once, TableWriters};
use processing::{classify_node, classify_relation, classify_way};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt::init();

    #[cfg(feature = "dhat")]
    let _dhat_profiler = dhat::Profiler::new_heap();
    mem_snapshot("startup");

    let cfg = Config::parse();

    if cfg.source == config::Source::Pbf {
        anyhow::ensure!(!cfg.pbf_file.is_empty(), "a .osm.pbf file is required for --source pbf");
    }
    if cfg.source == config::Source::Csv {
        anyhow::ensure!(
            cfg.output == Output::Csv,
            "--source csv only supports --output csv — it never has geometry to build a `pg`/`geojsonseq` output from (see `csv_source`'s own doc)"
        );
    }

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
    // Paired with whether each topic emits an `id` column (see `TopicSpec::id_type`) — the tag
    // table's DDL, COPY column list and uniqueness index all differ when a topic drops it.
    let tag_tables: Vec<(&str, bool)> =
        table_refs.iter().zip(&runners).map(|(&t, r)| (t, r.spec.id_type.emits_column())).collect();
    let emits_id: Vec<bool> = runners.iter().map(|r| r.spec.id_type.emits_column()).collect();

    for r in &runners {
        info!(
            "Loaded topic '{}' ({} categories, {} producers)",
            r.table(),
            r.categories.values().map(|c| c.categories.len()).sum::<usize>(),
            r.default_producers.len(),
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
    // Only safe when *every* topic rejects an untagged node — one `accept_all` (or one filter
    // satisfied by a tag's absence) and the whole optimization is off. See
    // `TopicRunner::skips_untagged` for why this is decided by probing the real pipeline.
    let skip_untagged_nodes = runners.iter().all(|r| r.skips_untagged(ElementKind::Node));

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
        .chain(plan.relation_line_topics.iter().map(|&i| (schema::relation_geom_table(table_refs[i]), schema::GeomTableShape::MultiLineString)))
        .chain(plan.relation_point_topics.iter().map(|&i| (schema::relation_point_table(table_refs[i]), schema::GeomTableShape::Point)))
        .chain(plan.relation_polygon_topics.iter().map(|&i| (schema::relation_polygon_table(table_refs[i]), schema::GeomTableShape::Polygon)))
        .collect();
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
            let (pool, k) = db::backend::setup(&cfg, &tag_tables, &geom_tables, plan.any_way_graph, n, extra_tables).await?;
            (Some(pool), k)
        }
        Output::Csv => {
            std::fs::create_dir_all(&cfg.out_dir)
                .with_context(|| format!("creating output dir {}", cfg.out_dir))?;
            info!("CSV output → {}/ (one file per tag table + edges.csv)", cfg.out_dir);
            (None, 1)
        }
        Output::GeoJson => {
            std::fs::create_dir_all(&cfg.out_dir)
                .with_context(|| format!("creating output dir {}", cfg.out_dir))?;
            info!("GeoJSON output → {}/ (one {{topic}}.geojson FeatureCollection per topic)", cfg.out_dir);
            (None, 1)
        }
        Output::GeoJsonSeq => {
            std::fs::create_dir_all(&cfg.out_dir)
                .with_context(|| format!("creating output dir {}", cfg.out_dir))?;
            info!("GeoJSONSeq output → {}/ (one {{topic}}.geojsonseq Feature stream per topic)", cfg.out_dir);
            (None, 1)
        }
        Output::Parquet => {
            std::fs::create_dir_all(&cfg.out_dir)
                .with_context(|| format!("creating output dir {}", cfg.out_dir))?;
            info!("Parquet output → {}/ (CSV staging files + one {{topic}}.parquet per topic)", cfg.out_dir);
            (None, 1)
        }
    };

    info!("Reading + processing PBF (streaming): {}", cfg.pbf_file);
    let t0 = std::time::Instant::now();

    // Spawn `w` writers per tag table + `w` for each shared table. For Postgres these are sharded
    // COPY connections (rows round-robined for k-way parallel serialization + ingest); for CSV, w=1,
    // so one buffered-file writer per table.
    let out_dir = PathBuf::from(&cfg.out_dir);
    let writers = Arc::new(TableWriters::spawn(cfg.output, &pool, &out_dir, w, &tables, &table_refs, &emits_id, &plan));
    mem_snapshot("config+writers");

    // Arc'd here (rather than right before the PBF select phase, where this used to live) so the
    // postgis live-editor branch below — which needs neither the PBF reader nor its callbacks —
    // can share both without duplicating the wrap.
    let plan = Arc::new(plan);
    let runners = Arc::new(runners);

    if cfg.source == config::Source::Csv {
        // Live-editor path: no PBF, no geometry at all — read ways' tags as CSV from stdin and run
        // just the tag/filter/producer pipeline (see `csv_source`'s own doc for why this exists;
        // the caller is responsible for joining the resulting `<topic>.csv` tag rows back to
        // whatever geometry it already holds).
        let n = osmnexus::csv_source::run(runners.clone(), writers.clone()).await?;
        info!("Classified {n} ways from CSV stdin source");

        let writers = Arc::try_unwrap(writers)
            .unwrap_or_else(|_| unreachable!("csv_source::run's writers clone is dropped by the time it returns"));
        let (writers, _select_counts) = writers.finish_select().await?;
        writers.finish_materialize(plan.any_way_graph).await?;
        return Ok(());
    }

    // Select phase: the reader decodes the PBF once and drives the classify callbacks below,
    // streaming tag rows out as it goes (side effect — see `osm::reader`'s own doc for why that
    // can't wait), and returns a `SelectionContext` once finished. Runs on the blocking pool
    // (CPU-bound rayon work).
    let pbf_file = cfg.pbf_file.clone();
    // Shared, thread-safe state captured by the reader's callbacks (called from rayon workers).
    let producer_runners = runners.clone();
    let producer_plan = plan.clone();
    let producer_writers = writers.clone();
    let select_task = tokio::task::spawn_blocking(move || -> anyhow::Result<osm::reader::SelectionContext> {
        let runners = producer_runners;
        let plan = producer_plan;
        let writers = producer_writers;
        // Ways pass: emit tag rows; a way is kept purely by its own tag classification — relation
        // membership has no bearing here (see `osm::reader::Callbacks::classify_way`'s own doc).
        // The returned mask becomes `SelectionContext::way_refs`' per-way keep mask.
        let classify_way_cb = {
            let (runners, writers) = (runners.clone(), writers.clone());
            move |wd: &WayData| -> Option<u32> {
                let out = classify_way(&runners, wd);
                let kept_by_topic = writers.route_tag(out.topic_rows);
                kept_by_topic.then_some(out.mask)
            }
        };
        // Relations pass: emit relation tag rows + `relation_members` links; return the keep mask.
        // Fully independent of the ways pass for classification purposes — a kept relation's
        // member ways are recorded into `SelectionContext::rel_members` by the reader itself
        // (regardless of whether any topic wants relation *geometry*; that decision is
        // `geom::materialize`'s, using `plan`, not made here).
        let classify_rel_cb = {
            let (runners, writers) = (runners.clone(), writers.clone());
            move |rd: &RelData| -> Option<u32> {
                let rows = classify_relation(&runners, rd);
                let mut mask = 0u32;
                for (i, r) in rows.iter().enumerate() {
                    if !r.is_empty() {
                        mask |= 1 << i;
                    }
                }
                let kept = writers.route_tag(rows);
                if kept && !rd.member_ways.is_empty() {
                    let links: Vec<MemberRow> = rd
                        .member_ways
                        .iter()
                        .map(|&(wid, _)| MemberRow { relation_osm_id: rd.id, way_osm_id: wid })
                        .collect();
                    writers.route_member(links);
                }
                kept.then_some(mask)
            }
        };
        // Nodes pass: emit node tag rows; also builds + routes this node's own point row right here
        // — a node is a leaf, its point shape needs nothing `SelectionContext` provides, so there's
        // no reason to defer it to the materialize phase the way way/relation geometry is deferred.
        // The callback's return value becomes `SelectionContext::selected` (forced graph cut
        // point) — true only when a *matching* topic declared `"geometry": {"node": ["graph"]}`
        // (`plan.node_graph_mask`), not merely "some topic classified this node" — a point-only (or
        // bare) node topic never affects how ways get cut.
        let classify_node_cb = {
            let (runners, writers, plan) = (runners.clone(), writers.clone(), plan.clone());
            move |nd: &NodeData| -> bool {
                let rows = classify_node(&runners, nd);
                let mut mask = 0u32;
                for (i, r) in rows.iter().enumerate() {
                    if !r.is_empty() {
                        mask |= 1 << i;
                    }
                }
                let kept = writers.route_tag(rows);
                if kept {
                    if let Some(row) = geom::materialize::node_point(nd.id, nd.lon, nd.lat, &plan) {
                        writers.route_node_point(mask, row, &plan);
                    }
                }
                kept && (mask & plan.node_graph_mask != 0)
            }
        };
        osm::reader::stream_osm(
            &pbf_file,
            osm::reader::Callbacks {
                has_relations,
                classify_rel: classify_rel_cb,
                relation_geom_mask: plan.relation_geom_mask,
                classify_way: classify_way_cb,
                has_nodes,
                classify_node: classify_node_cb,
                skip_untagged_nodes,
                needs_graph: plan.any_way_graph,
            },
        )
    });

    // Await the select phase: it drops its `writers` clone (closing the tag/member channels — the
    // point-table writers are shared with the materialize phase below via the outer `writers`
    // handle, so they stay open until that phase finishes too).
    let ctx = select_task.await.context("select-phase task panicked")??;

    let writers = Arc::try_unwrap(writers)
        .unwrap_or_else(|_| unreachable!("select_task's writers clone is dropped by the time it returns"));
    let (writers, _select_counts) = writers.finish_select().await?;
    info!("Select phase time: {:.1}s", t0.elapsed().as_secs_f32());
    mem_snapshot("select");

    // Materialize phase: resolve way/relation geometry from `ctx` and route every row to its
    // writer channel — see `geom::materialize::run`'s own doc. Runs on the blocking pool (rayon
    // work inside); way rows are routed straight from `run`'s own parallel resolution pass (one
    // `writers.route_way` call per way, from whichever rayon worker resolved it) instead of being
    // collected into a `Vec` and drained afterward — keeps every way's output rows from being
    // resident in memory at once on top of `ctx.node_coords`/the per-way coordinate cache.
    let t_mat = std::time::Instant::now();
    let materialize_plan = plan.clone();
    let writers = Arc::new(writers);
    let materialize_writers = writers.clone();
    let relations_batch = tokio::task::spawn_blocking(move || {
        let writers = materialize_writers;
        let m = geom::materialize::run(&ctx, &materialize_plan, |_way_id, mask, g| {
            writers.route_way(mask, g, &materialize_plan);
        });
        writers.route_node_rows(m.node_rows);
        m.relations
    })
    .await
    .context("materialize-phase task panicked")?;
    info!("Materialize phase time: {:.1}s", t_mat.elapsed().as_secs_f32());

    let writers = Arc::try_unwrap(writers)
        .unwrap_or_else(|_| unreachable!("materialize task's writers clone is dropped by the time it returns"));
    writers.finish_materialize(plan.any_way_graph).await?;
    mem_snapshot("materialize");

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

    if cfg.output == Output::Pg {
        db::backend::finalize(&cfg, pool.as_ref().unwrap(), &tag_tables, &geom_tables, plan.any_way_graph, &runners).await?;
    }

    if cfg.output == Output::GeoJson {
        info!("Building GeoJSON from CSV output...");
        output::geojson::write_geojson_from_csv(&out_dir, &tables)?;
        for table in &tables {
            info!("Wrote {}/{table}.geojson", cfg.out_dir);
        }
    }

    if cfg.output == Output::GeoJsonSeq {
        info!("Building GeoJSONSeq from CSV output...");
        output::geojson::write_geojsonseq_from_csv(&out_dir, &tables)?;
        for table in &tables {
            info!("Wrote {}/{table}.geojsonseq", cfg.out_dir);
        }
    }

    if cfg.output == Output::Parquet {
        info!("Building Parquet from CSV output...");
        output::parquet::write_parquet_from_csv(&out_dir, &tables)?;
        for table in &tables {
            info!("Wrote {}/{table}.parquet", cfg.out_dir);
        }
    }

    mem_snapshot("relation-geom+finalize");
    info!("Done. Total: {:.1}s", t0.elapsed().as_secs_f32());
    Ok(())
}
