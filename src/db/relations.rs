use tokio_postgres::Client;

use super::schema::{MEMBER_TABLE, RELATION_GEOM_TABLE, WAY_GEOM_TABLE};

/// Build `RELATION_GEOM_TABLE`: one merged-linestring row per kept relation, from its member ways'
/// whole-way geometries (`relation_members` ⋈ `WAY_GEOM_TABLE`, `ST_Collect` + `ST_LineMerge`
/// grouped by `relation_osm_id`).
///
/// Relations are classified before any way geometry exists (see `osm::reader`), so this can only run
/// as a post-COPY SQL step, not during the streaming pass.
///
/// When `has_way_geometries` is `false` (i.e. `--emit-way-geometries` was not passed), `WAY_GEOM_TABLE`
/// doesn't exist yet — it's built here on the fly by merging `EDGE_TABLE`'s split segments back into
/// whole ways, then dropped again once `RELATION_GEOM_TABLE` is populated, so nothing lingers beyond
/// what was asked for.
pub async fn materialize_relation_geometries(client: &Client, has_way_geometries: bool) -> anyhow::Result<()> {
    use super::schema::EDGE_TABLE;

    if !has_way_geometries {
        client
            .batch_execute(&format!(
                "DROP TABLE IF EXISTS {WAY_GEOM_TABLE}; \
                 CREATE TABLE {WAY_GEOM_TABLE} AS \
                 SELECT osm_id, ST_LineMerge(ST_Collect(geom)) AS geom, sum(length_m) AS length_m \
                 FROM {EDGE_TABLE} GROUP BY osm_id"
            ))
            .await?;
    }

    client
        .batch_execute(&format!(
            "DROP TABLE IF EXISTS {RELATION_GEOM_TABLE}; \
             CREATE TABLE {RELATION_GEOM_TABLE} AS \
             SELECT rm.relation_osm_id AS osm_id, ST_LineMerge(ST_Collect(w.geom)) AS geom \
             FROM {MEMBER_TABLE} AS rm \
             JOIN {WAY_GEOM_TABLE} AS w ON w.osm_id = rm.way_osm_id \
             GROUP BY rm.relation_osm_id"
        ))
        .await?;
    client
        .batch_execute(&format!(
            "CREATE INDEX ON {RELATION_GEOM_TABLE} USING GIST (geom); \
             CREATE INDEX ON {RELATION_GEOM_TABLE} (osm_id)"
        ))
        .await?;

    if !has_way_geometries {
        client.batch_execute(&format!("DROP TABLE {WAY_GEOM_TABLE}")).await?;
    }

    Ok(())
}
