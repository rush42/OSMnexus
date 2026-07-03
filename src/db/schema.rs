use deadpool_postgres::Pool;
use tokio_postgres::Client;

/// The single shared geometry table. A way's geometry (and its intersection split, computed from
/// the *global* node use-counts) is topic-independent, so every topic's tag table joins to this one
/// table on `osm_id`. Conceptually: one extracted graph, with the per-topic tag tables as disjoint
/// attribute layers over it.
pub const GEOM_TABLE: &str = "geometries";

/// Tag table: one row per (way, side, prefix). No geometry — that lives in the shared `GEOM_TABLE`,
/// joined at tile-materialization time on `osm_id`.
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

/// Shared geometry table: one row per (way, variant, segment). `variant` is `way` (whole way,
/// `seg_idx` NULL) or `split` (one row per intersection sub-linestring). `start_id`/`end_id`
/// are the OSM node ids at each end of the (sub-)linestring — the seeds of the graph topology.
fn create_geom_table_sql() -> String {
    format!(r#"
CREATE TABLE IF NOT EXISTS {GEOM_TABLE} (
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

fn drop_tag_indexes_sql(table: &str) -> String {
    format!(
        "DROP INDEX IF EXISTS {table}_id_idx;\n\
         DROP INDEX IF EXISTS {table}_osm_id_idx;\n\
         DROP INDEX IF EXISTS {table}_minzoom_idx"
    )
}

fn drop_geom_indexes_sql() -> String {
    format!(
        "DROP INDEX IF EXISTS {GEOM_TABLE}_geom_idx;\n\
         DROP INDEX IF EXISTS {GEOM_TABLE}_osm_id_idx"
    )
}

/// Tag-table indexes: unique feature id, join key on osm_id, minzoom filter.
fn tag_index_stmts(table: &str) -> [String; 3] {
    [
        format!("CREATE UNIQUE INDEX IF NOT EXISTS {table}_id_idx ON {table} (id)"),
        format!("CREATE INDEX IF NOT EXISTS {table}_osm_id_idx ON {table} (osm_id)"),
        format!("CREATE INDEX IF NOT EXISTS {table}_minzoom_idx ON {table} (minzoom)"),
    ]
}

/// Geom-table indexes: spatial index + join key on osm_id.
fn geom_index_stmts() -> [String; 2] {
    [
        format!("CREATE INDEX IF NOT EXISTS {GEOM_TABLE}_geom_idx ON {GEOM_TABLE} USING GIST (geom)"),
        format!("CREATE INDEX IF NOT EXISTS {GEOM_TABLE}_osm_id_idx ON {GEOM_TABLE} (osm_id)"),
    ]
}

pub async fn create_tables(client: &Client, tables: &[&str]) -> anyhow::Result<()> {
    for table in tables {
        client.batch_execute(&create_tag_table_sql(table)).await?;
    }
    client.batch_execute(&create_geom_table_sql()).await?;
    Ok(())
}

pub async fn truncate_tables(client: &Client, tables: &[&str]) -> anyhow::Result<()> {
    let mut all: Vec<String> = tables.iter().map(|t| t.to_string()).collect();
    all.push(GEOM_TABLE.to_string());
    client.batch_execute(&format!("TRUNCATE TABLE {}", all.join(", "))).await?;
    Ok(())
}

pub async fn drop_indexes(client: &Client, tables: &[&str]) -> anyhow::Result<()> {
    for table in tables {
        client.batch_execute(&drop_tag_indexes_sql(table)).await?;
    }
    client.batch_execute(&drop_geom_indexes_sql()).await?;
    Ok(())
}

/// Build the indexes, one build-unit per pooled connection (= one Postgres backend each)
/// **concurrently**: each tag table is a unit, and the shared geom table is one more. Indexes
/// *within* a unit run serially (avoids concurrent re-scans of the same heap).
///
/// Two levers, both measured on a Germany import (8 cores): each session raises
/// `maintenance_work_mem` + `max_parallel_maintenance_workers` (parallel sort / GiST build), and
/// the units build in parallel so the small tables hide under the dominant geom GiST.
pub async fn create_indexes(pool: &Pool, tables: &[&str]) -> anyhow::Result<()> {
    // One build unit per tag table, plus the shared geom table.
    let mut units: Vec<Vec<String>> = tables.iter().map(|t| tag_index_stmts(t).to_vec()).collect();
    units.push(geom_index_stmts().to_vec());

    let handles: Vec<_> = units
        .into_iter()
        .map(|stmts| {
            let pool = pool.clone();
            tokio::spawn(async move {
                let client = pool.get().await?;
                // Session-local: let each build use parallel workers + more sort memory.
                client
                    .batch_execute(
                        "SET maintenance_work_mem = '1GB'; \
                         SET max_parallel_maintenance_workers = 4",
                    )
                    .await?;
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
