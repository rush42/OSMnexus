use deadpool_postgres::Pool;
use tokio_postgres::Client;

fn create_table_sql(table: &str) -> String {
    format!(r#"
CREATE TABLE IF NOT EXISTS {table} (
  osm_id    bigint,
  osm_type  text,
  id        text NOT NULL,
  osm       jsonb,
  derived   jsonb,
  private   jsonb,
  meta      jsonb,
  geom      geometry(LineString, 3857),
  minzoom   integer NOT NULL
)"#)
}

fn drop_indexes_sql(table: &str) -> String {
    format!(
        "DROP INDEX IF EXISTS {table}_geom_idx;\n\
         DROP INDEX IF EXISTS {table}_minzoom_idx;\n\
         DROP INDEX IF EXISTS {table}_id_idx"
    )
}

/// One `CREATE INDEX` statement per index, so each can run on its own connection (and thus
/// its own Postgres backend) concurrently.
fn create_index_stmts(table: &str) -> [String; 3] {
    [
        format!("CREATE INDEX IF NOT EXISTS {table}_geom_idx ON {table} USING GIST (geom)"),
        format!("CREATE INDEX IF NOT EXISTS {table}_minzoom_idx ON {table} (minzoom)"),
        format!("CREATE UNIQUE INDEX IF NOT EXISTS {table}_id_idx ON {table} (id)"),
    ]
}

pub async fn create_tables(client: &Client, tables: &[&str]) -> anyhow::Result<()> {
    for table in tables {
        client.batch_execute(&create_table_sql(table)).await?;
    }
    Ok(())
}

pub async fn truncate_tables(client: &Client, tables: &[&str]) -> anyhow::Result<()> {
    let list = tables.join(", ");
    client.batch_execute(&format!("TRUNCATE TABLE {list}")).await?;
    Ok(())
}

pub async fn drop_indexes(client: &Client, tables: &[&str]) -> anyhow::Result<()> {
    for table in tables {
        client.batch_execute(&drop_indexes_sql(table)).await?;
    }
    Ok(())
}

/// Build the per-table indexes, one table per pooled connection (= one Postgres backend
/// each) **concurrently across tables**, but the three indexes *within* a table **serially**.
///
/// Two levers, both measured on a Germany import (14.3M rows, 8 cores):
///   * Each session raises `maintenance_work_mem` + `max_parallel_maintenance_workers`, so a
///     single build fans out across cores (parallel sort / GiST build).
///   * Tables build in parallel, hiding the small tables' indexes under the dominant one.
///
/// Crucially the indexes of one table are *not* built concurrently: doing so re-scans the
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
                // Serial within the table — avoids concurrent re-scans of the same heap.
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
