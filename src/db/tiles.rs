use deadpool_postgres::Pool;
use tokio_postgres::Client;

use super::schema::EDGE_TABLE;

/// Tile-server output for a topic: join its tag table to the shared edge table on `osm_id`,
/// exposing attributes + geometry in one relation named `<topic>_tiles`.
///
/// A view is free and always reflects the latest import; a materialized table is a physical copy
/// with a GiST spatial index, which is what a tile server actually renders from (a view can't carry
/// a spatial index). `edges` fans each feature out across its intersection-split segments.
fn tile_select_sql(table: &str) -> String {
    format!(
        "SELECT \
           t.id, t.osm_id, t.osm_type, t.osm, t.derived, t.meta, t.minzoom, \
           g.seg_idx, g.start_id, g.end_id, g.length_m, g.total_length_m, g.geom \
         FROM {table} AS t \
         JOIN {EDGE_TABLE} AS g ON g.osm_id = t.osm_id"
    )
}

/// Create one `<topic>_tiles` view per topic. Cheap and always current.
pub async fn create_tile_views(client: &Client, tables: &[&str]) -> anyhow::Result<()> {
    for table in tables {
        let sql = format!(
            "CREATE OR REPLACE VIEW {table}_tiles AS {}",
            tile_select_sql(table)
        );
        client.batch_execute(&sql).await?;
    }
    Ok(())
}

/// Materialize one `<topic>_tiles` physical table per topic, then build its GiST spatial index and
/// minzoom index. Each topic builds on its own pooled connection concurrently (same pattern as
/// `create_indexes`), since the GiST build on the joined geometry is the dominant cost.
pub async fn materialize_tiles(pool: &Pool, tables: &[&str]) -> anyhow::Result<()> {
    let handles: Vec<_> = tables
        .iter()
        .map(|table| {
            let table = table.to_string();
            let pool = pool.clone();
            tokio::spawn(async move {
                let client = pool.get().await?;
                client
                    .batch_execute(
                        "SET maintenance_work_mem = '1GB'; \
                         SET max_parallel_maintenance_workers = 4",
                    )
                    .await?;
                let view = format!("{table}_tiles");
                client
                    .batch_execute(&format!("DROP TABLE IF EXISTS {view}"))
                    .await?;
                client
                    .batch_execute(&format!(
                        "CREATE TABLE {view} AS {}",
                        tile_select_sql(&table)
                    ))
                    .await?;
                client
                    .batch_execute(&format!(
                        "CREATE INDEX ON {view} USING GIST (geom); \
                         CREATE INDEX ON {view} (minzoom); \
                         ANALYZE {view}"
                    ))
                    .await?;
                anyhow::Ok(())
            })
        })
        .collect();

    for h in handles {
        h.await??;
    }
    Ok(())
}
