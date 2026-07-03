use deadpool_postgres::Pool;
use tokio_postgres::Client;

/// The geometry table paired with a tag table `t`.
fn geom_table(t: &str) -> String {
    format!("{t}_geom")
}

/// Tag table: one row per (way, side, prefix). No geometry — that lives in the paired
/// `<table>_geom` table, joined at tile-materialization time on `osm_id`.
fn create_tag_table_sql(table: &str) -> String {
    format!(r#"
CREATE TABLE IF NOT EXISTS {table} (
  osm_id    bigint,
  osm_type  text,
  id        text NOT NULL,
  osm       jsonb,
  derived   jsonb,
  private   jsonb,
  meta      jsonb,
  minzoom   integer NOT NULL
)"#)
}

/// Geometry table: one row per (way, variant, segment). `variant` is `way` (whole way,
/// `seg_idx` NULL) or `split` (one row per intersection sub-linestring). `start_id`/`end_id`
/// are the OSM node ids at each end of the (sub-)linestring — the seeds of the graph topology.
fn create_geom_table_sql(table: &str) -> String {
    let geom = geom_table(table);
    format!(r#"
CREATE TABLE IF NOT EXISTS {geom} (
  osm_id         bigint,
  variant        text NOT NULL,
  seg_idx        integer,
  start_id       bigint,
  end_id         bigint,
  geom           geometry(LineString, 3857),
  length_m       double precision,
  total_length_m double precision
)"#)
}

fn drop_indexes_sql(table: &str) -> String {
    let geom = geom_table(table);
    format!(
        "DROP INDEX IF EXISTS {table}_id_idx;\n\
         DROP INDEX IF EXISTS {table}_osm_id_idx;\n\
         DROP INDEX IF EXISTS {table}_minzoom_idx;\n\
         DROP INDEX IF EXISTS {geom}_geom_idx;\n\
         DROP INDEX IF EXISTS {geom}_osm_id_idx"
    )
}

/// One `CREATE INDEX` statement per index, so each can run on its own connection (and thus
/// its own Postgres backend) concurrently. Covers both the tag table and its geom table.
fn create_index_stmts(table: &str) -> [String; 5] {
    let geom = geom_table(table);
    [
        // Tag table: unique feature id, join key on osm_id, minzoom filter.
        format!("CREATE UNIQUE INDEX IF NOT EXISTS {table}_id_idx ON {table} (id)"),
        format!("CREATE INDEX IF NOT EXISTS {table}_osm_id_idx ON {table} (osm_id)"),
        format!("CREATE INDEX IF NOT EXISTS {table}_minzoom_idx ON {table} (minzoom)"),
        // Geom table: spatial index + join key on osm_id.
        format!("CREATE INDEX IF NOT EXISTS {geom}_geom_idx ON {geom} USING GIST (geom)"),
        format!("CREATE INDEX IF NOT EXISTS {geom}_osm_id_idx ON {geom} (osm_id)"),
    ]
}

pub async fn create_tables(client: &Client, tables: &[&str]) -> anyhow::Result<()> {
    for table in tables {
        client.batch_execute(&create_tag_table_sql(table)).await?;
        client.batch_execute(&create_geom_table_sql(table)).await?;
    }
    Ok(())
}

pub async fn truncate_tables(client: &Client, tables: &[&str]) -> anyhow::Result<()> {
    let all: Vec<String> = tables
        .iter()
        .flat_map(|t| [t.to_string(), geom_table(t)])
        .collect();
    let list = all.join(", ");
    client.batch_execute(&format!("TRUNCATE TABLE {list}")).await?;
    Ok(())
}

pub async fn drop_indexes(client: &Client, tables: &[&str]) -> anyhow::Result<()> {
    for table in tables {
        client.batch_execute(&drop_indexes_sql(table)).await?;
    }
    Ok(())
}

/// Build the per-table indexes, one table-pair per pooled connection (= one Postgres backend
/// each) **concurrently across topics**, but the indexes *within* a topic **serially**.
///
/// Two levers, both measured on a Germany import (14.3M rows, 8 cores):
///   * Each session raises `maintenance_work_mem` + `max_parallel_maintenance_workers`, so a
///     single build fans out across cores (parallel sort / GiST build).
///   * Topics build in parallel, hiding the small tables' indexes under the dominant one.
///
/// Crucially the indexes of one topic are *not* built concurrently: doing so re-scans the
/// same large heap N times at once and thrashes the cores — on Germany that was a regression
/// (220s) vs this scheme (170s, down from 193s serial). Berlin (437k rows) goes 6.0s → 4.5s.
pub async fn create_indexes(pool: &Pool, tables: &[&str]) -> anyhow::Result<()> {
    let handles: Vec<_> = tables
        .iter()
        .map(|table| {
            let pool = pool.clone();
            let stmts = create_index_stmts(table);
            tokio::spawn(async move {
                let client = pool.get().await?;
                // Session-local: let each build use parallel workers + more sort memory.
                client
                    .batch_execute(
                        "SET maintenance_work_mem = '1GB'; \
                         SET max_parallel_maintenance_workers = 4",
                    )
                    .await?;
                // Serial within the topic — avoids concurrent re-scans of the same heap.
                for stmt in stmts {
                    client.batch_execute(&stmt).await?;
                }
                anyhow::Ok(())
            })
        })
        .collect();

    for h in handles {
        h.await??;
    }
    Ok(())
}
