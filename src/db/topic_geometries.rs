//! Per-topic `relation` linestring materialization (`{table}_relation_geom` — see
//! `TopicRunner::wants_relation_linestring`). A relation is classified before any member way's
//! geometry is resolved (see `osm::reader`), so — unlike a topic's *way* linestrings, which are
//! computed and routed during streaming (`main.rs`'s `build_geom_cb`) — this can only run as a
//! post-COPY SQL step, joining each topic's kept relations to their member ways' geometry through
//! `MEMBER_TABLE`.

use tokio_postgres::Client;

use super::schema::{EDGE_TABLE, MEMBER_TABLE};

/// Internal staging table: every kept way's whole (unsplit) linestring, reconstructed once by
/// merging `EDGE_TABLE`'s intersection-split segments back together — the source every topic
/// wanting relation linestrings joins against. Never exposed as an output table itself (unlike a
/// topic's own `{table}_geom`, which is built directly from streaming-time rows, not this); built
/// before, and dropped after, materializing every topic's `{table}_relation_geom`.
const STAGING_TABLE: &str = "_way_geom_staging";

pub async fn ensure_way_geom_staging(client: &Client) -> anyhow::Result<()> {
    client
        .batch_execute(&format!(
            "DROP TABLE IF EXISTS {STAGING_TABLE}; \
             CREATE TABLE {STAGING_TABLE} AS \
             SELECT osm_id, ST_LineMerge(ST_Collect(geom)) AS geom \
             FROM {EDGE_TABLE} GROUP BY osm_id"
        ))
        .await?;
    Ok(())
}

pub async fn drop_way_geom_staging(client: &Client) -> anyhow::Result<()> {
    client.batch_execute(&format!("DROP TABLE IF EXISTS {STAGING_TABLE}")).await?;
    Ok(())
}

/// Build `{table}_relation_geom`: one merged-linestring row per this topic's kept relations, from
/// its member ways' geometries (`MEMBER_TABLE` ⋈ `STAGING_TABLE`, `ST_Collect` + `ST_LineMerge`
/// grouped by `relation_osm_id`, restricted to relation rows this topic actually kept).
/// `ensure_way_geom_staging` must have run first.
pub async fn materialize_relation_linestrings(
    client: &Client,
    table: &str,
    create_index: bool,
) -> anyhow::Result<()> {
    let out = super::schema::relation_geom_table(table);
    client
        .batch_execute(&format!(
            "DROP TABLE IF EXISTS {out}; \
             CREATE TABLE {out} AS \
             SELECT rm.relation_osm_id AS osm_id, ST_LineMerge(ST_Collect(w.geom)) AS geom \
             FROM {MEMBER_TABLE} rm \
             JOIN {STAGING_TABLE} w ON w.osm_id = rm.way_osm_id \
             JOIN {table} t ON t.osm_id = rm.relation_osm_id AND t.osm_type = 'R' \
             GROUP BY rm.relation_osm_id"
        ))
        .await?;
    if create_index {
        client
            .batch_execute(&format!(
                "CREATE INDEX ON {out} USING GIST (geom); CREATE INDEX ON {out} (osm_id)"
            ))
            .await?;
    }
    Ok(())
}
