//! Postgres backend lifecycle: schema setup (create/truncate/drop-indexes + pool sizing) before the
//! streaming pipeline runs, and index creation + topic-edge materialization after it finishes.
//! CSV/GeoJSON output needs neither — `main.rs` calls this module only in the `Output::Pg` branch,
//! everything else (writer channels, select/materialize) is output-backend-agnostic.

use anyhow::Context;
use deadpool_postgres::Pool;
use tracing::info;

use crate::config::Config;
use crate::db::pool::build_pool;
use crate::db::schema;
use crate::db::topic_edges;
use crate::topic::TopicRunner;

/// `tag_tables` is `(table name, whether it emits the `id` column)` — see `TopicSpec::id_type`.
///
/// Connect, create/truncate tables + drop indexes, and size the pool for `n_tag_tables` tag tables
/// plus `extra_tables` shared/geometry tables at `cfg.db_writers` connections each. Returns the
/// pool and `w` (writers per table) for the caller to pass to `TableWriters::spawn`.
pub async fn setup(
    cfg: &Config,
    tag_tables: &[(&str, bool)],
    geom_tables: &[String],
    emit_graph: bool,
    n_tag_tables: usize,
    extra_tables: usize,
) -> anyhow::Result<(Pool, usize)> {
    info!("Connecting to database {}@{}/{}", cfg.db_user, cfg.db_host, cfg.db_name);
    let pool = build_pool(cfg)?;
    let client = pool.get().await.context("getting DB connection")?;
    info!("Setting up schema...");
    let table_refs: Vec<&str> = tag_tables.iter().map(|&(t, _)| t).collect();
    schema::create_tables(&client, tag_tables, geom_tables, emit_graph).await?;
    if cfg.truncate {
        schema::truncate_tables(&client, &table_refs, geom_tables, emit_graph).await?;
    }
    schema::drop_indexes(&client, &table_refs, geom_tables, emit_graph).await?;
    drop(client);
    let k = cfg.db_writers.max(1);
    // Pool must supply every writer connection at once: k per tag table + k per extra table.
    pool.resize((n_tag_tables + extra_tables) * k + 2);
    info!("Postgres output · {k} COPY connection(s) per table");
    Ok((pool, k))
}

/// After the streaming pipeline finishes: build indexes (if `cfg.create_index`) and materialize
/// each graph-wanting topic's routing-edge table (`{table}_edge`).
pub async fn finalize(
    cfg: &Config,
    pool: &Pool,
    tag_tables: &[(&str, bool)],
    geom_tables: &[String],
    emit_graph: bool,
    runners: &[TopicRunner],
) -> anyhow::Result<()> {
    if cfg.create_index {
        info!("Creating indexes...");
        let t_idx = std::time::Instant::now();
        schema::create_indexes(pool, tag_tables, geom_tables, emit_graph).await?;
        info!("Index creation: {:.1}s", t_idx.elapsed().as_secs_f32());
    } else {
        info!("Skipping index creation (pass --create-index to enable)");
    }

    let client = pool.get().await?;
    for r in runners.iter().filter(|r| r.wants_way_graph()) {
        info!("Materializing graph edges → {}_edge", r.table());
        topic_edges::materialize(&client, r.table(), cfg.topic_edges, cfg.create_index).await?;
    }
    Ok(())
}
