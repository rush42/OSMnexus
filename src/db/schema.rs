use deadpool_postgres::Pool;
use tokio_postgres::Client;

/// The single shared graph-edge table: one row per intersection-split sub-linestring of every kept
/// way (the extracted graph). Topic-independent, so every topic's tag table joins to this one table
/// on `osm_id`. Conceptually: one extracted graph, with the per-topic tag tables as disjoint
/// attribute layers over it.
pub const EDGE_TABLE: &str = "edges";

/// Relation → member-way link table. Relations have no materialized geometry; a relation's tag row
/// joins here to reach its member ways' rows in `EDGE_TABLE` (or a topic's own `{table}_geom`).
pub const MEMBER_TABLE: &str = "relation_members";

/// Graph-vertex table: one row per node referenced as a `start_id`/`end_id` in `EDGE_TABLE` — shared
/// between ≥2 ways, a way endpoint, or forced by a node classifier. Always populated (this is the
/// `edges.start_id`/`end_id` ↔ `osm_id` mapping, load-bearing rather than optional). `id` is the
/// internal sequential id `edges` joins on; `osm_id` is the original OSM node id.
pub const NODE_TABLE: &str = "nodes";

/// This topic's whole-way linestring table name (populated only for topics declaring
/// `"geometry": { "way": ["linestring"] }` — see `TopicRunner::wants_way_linestring`).
pub fn way_geom_table(table: &str) -> String {
    format!("{table}_geom")
}

/// This topic's merged relation-linestring table name (populated only for topics declaring
/// `"geometry": { "relation": ["linestring"] }` — see `db::topic_geometries`).
pub fn relation_geom_table(table: &str) -> String {
    format!("{table}_relation_geom")
}

fn create_member_table_sql() -> String {
    format!(r#"
CREATE TABLE IF NOT EXISTS {MEMBER_TABLE} (
  relation_osm_id bigint,
  way_osm_id      bigint
)"#)
}

fn drop_member_indexes_sql() -> String {
    format!(
        "DROP INDEX IF EXISTS {MEMBER_TABLE}_relation_idx;\n\
         DROP INDEX IF EXISTS {MEMBER_TABLE}_way_idx"
    )
}

fn member_index_stmts() -> [String; 2] {
    [
        format!("CREATE INDEX IF NOT EXISTS {MEMBER_TABLE}_relation_idx ON {MEMBER_TABLE} (relation_osm_id)"),
        format!("CREATE INDEX IF NOT EXISTS {MEMBER_TABLE}_way_idx ON {MEMBER_TABLE} (way_osm_id)"),
    ]
}

/// Tag table: one row per (way, side, prefix). No geometry — that lives in `EDGE_TABLE`, joined at
/// tile-materialization time on `osm_id`.
fn create_tag_table_sql(table: &str) -> String {
    format!(r#"
CREATE TABLE IF NOT EXISTS {table} (
  osm_id    bigint,
  osm_type  text,
  id        text NOT NULL,
  category  text NOT NULL,
  produced    jsonb,
  annotations jsonb,
  meta        jsonb
)"#)
}

/// Shared graph-edge table: one row per (way, segment). `start_id`/`end_id` are internal node ids
/// (see `NODE_TABLE`), the seeds of the graph topology — pgRouting-style `source`/`target`.
/// `cost`/`reverse_cost` are always equal (symmetric/undirected) — this pipeline doesn't bake in
/// oneway semantics; a consumer wanting directed routing derives it at query time from the tag
/// table's `oneway` field (joined on `osm_id`).
fn create_edge_table_sql() -> String {
    format!(r#"
CREATE TABLE IF NOT EXISTS {EDGE_TABLE} (
  osm_id         bigint,
  seg_idx        integer,
  start_id       bigint,
  end_id         bigint,
  geom           geometry(LineString, 3857),
  length_m       double precision,
  total_length_m double precision,
  cost           double precision,
  reverse_cost   double precision
)"#)
}

fn create_way_geom_table_sql(table: &str) -> String {
    let way_geom = way_geom_table(table);
    format!(r#"
CREATE TABLE IF NOT EXISTS {way_geom} (
  osm_id   bigint,
  geom     geometry(LineString, 3857),
  length_m double precision
)"#)
}

fn create_node_table_sql() -> String {
    format!(r#"
CREATE TABLE IF NOT EXISTS {NODE_TABLE} (
  id     bigint,
  osm_id bigint,
  geom   geometry(Point, 3857)
)"#)
}

fn drop_tag_indexes_sql(table: &str) -> String {
    format!(
        "DROP INDEX IF EXISTS {table}_id_idx;\n\
         DROP INDEX IF EXISTS {table}_osm_id_idx"
    )
}

fn drop_edge_indexes_sql() -> String {
    format!(
        "DROP INDEX IF EXISTS {EDGE_TABLE}_geom_idx;\n\
         DROP INDEX IF EXISTS {EDGE_TABLE}_osm_id_idx"
    )
}

fn drop_way_geom_indexes_sql(table: &str) -> String {
    let way_geom = way_geom_table(table);
    format!(
        "DROP INDEX IF EXISTS {way_geom}_geom_idx;\n\
         DROP INDEX IF EXISTS {way_geom}_osm_id_idx"
    )
}

fn drop_node_indexes_sql() -> String {
    format!(
        "DROP INDEX IF EXISTS {NODE_TABLE}_geom_idx;\n\
         DROP INDEX IF EXISTS {NODE_TABLE}_osm_id_idx;\n\
         DROP INDEX IF EXISTS {NODE_TABLE}_id_idx"
    )
}

/// Tag-table indexes: unique feature id, join key on osm_id.
fn tag_index_stmts(table: &str) -> [String; 2] {
    [
        format!("CREATE UNIQUE INDEX IF NOT EXISTS {table}_id_idx ON {table} (id)"),
        format!("CREATE INDEX IF NOT EXISTS {table}_osm_id_idx ON {table} (osm_id)"),
    ]
}

/// Edge-table indexes: spatial index + join key on osm_id.
fn edge_index_stmts() -> [String; 2] {
    [
        format!("CREATE INDEX IF NOT EXISTS {EDGE_TABLE}_geom_idx ON {EDGE_TABLE} USING GIST (geom)"),
        format!("CREATE INDEX IF NOT EXISTS {EDGE_TABLE}_osm_id_idx ON {EDGE_TABLE} (osm_id)"),
    ]
}

fn way_geom_index_stmts(table: &str) -> [String; 2] {
    let way_geom = way_geom_table(table);
    [
        format!("CREATE INDEX IF NOT EXISTS {way_geom}_geom_idx ON {way_geom} USING GIST (geom)"),
        format!("CREATE INDEX IF NOT EXISTS {way_geom}_osm_id_idx ON {way_geom} (osm_id)"),
    ]
}

fn node_index_stmts() -> [String; 3] {
    [
        format!("CREATE INDEX IF NOT EXISTS {NODE_TABLE}_geom_idx ON {NODE_TABLE} USING GIST (geom)"),
        format!("CREATE INDEX IF NOT EXISTS {NODE_TABLE}_osm_id_idx ON {NODE_TABLE} (osm_id)"),
        format!("CREATE UNIQUE INDEX IF NOT EXISTS {NODE_TABLE}_id_idx ON {NODE_TABLE} (id)"),
    ]
}

/// `way_geom_tables`: the (topic) table names whose `{table}_geom` whole-way-linestring table
/// should exist — i.e. topics declaring `"geometry": { "way": ["linestring"] }` (see
/// `TopicRunner::wants_way_linestring`). Threaded through `create_tables`/`truncate_tables`/
/// `drop_indexes`/`create_indexes` alongside the always-present `EDGE_TABLE` + `MEMBER_TABLE` +
/// `NODE_TABLE`, replacing the old global `Config::emit_way_geometries` flag.
pub async fn create_tables(client: &Client, tables: &[&str], way_geom_tables: &[&str]) -> anyhow::Result<()> {
    for table in tables {
        client.batch_execute(&create_tag_table_sql(table)).await?;
    }
    client.batch_execute(&create_edge_table_sql()).await?;
    client.batch_execute(&create_member_table_sql()).await?;
    client.batch_execute(&create_node_table_sql()).await?;
    for table in way_geom_tables {
        client.batch_execute(&create_way_geom_table_sql(table)).await?;
    }
    Ok(())
}

pub async fn truncate_tables(client: &Client, tables: &[&str], way_geom_tables: &[&str]) -> anyhow::Result<()> {
    let mut all: Vec<String> = tables.iter().map(|t| t.to_string()).collect();
    all.push(EDGE_TABLE.to_string());
    all.push(MEMBER_TABLE.to_string());
    all.push(NODE_TABLE.to_string());
    all.extend(way_geom_tables.iter().map(|t| way_geom_table(t)));
    client.batch_execute(&format!("TRUNCATE TABLE {}", all.join(", "))).await?;
    Ok(())
}

pub async fn drop_indexes(client: &Client, tables: &[&str], way_geom_tables: &[&str]) -> anyhow::Result<()> {
    for table in tables {
        client.batch_execute(&drop_tag_indexes_sql(table)).await?;
    }
    client.batch_execute(&drop_edge_indexes_sql()).await?;
    client.batch_execute(&drop_member_indexes_sql()).await?;
    client.batch_execute(&drop_node_indexes_sql()).await?;
    for table in way_geom_tables {
        client.batch_execute(&drop_way_geom_indexes_sql(table)).await?;
    }
    Ok(())
}

/// Build the indexes, one build-unit per pooled connection (= one Postgres backend each)
/// **concurrently**: each tag table is a unit, and the shared edge table is one more. Indexes
/// *within* a unit run serially (avoids concurrent re-scans of the same heap).
///
/// Two levers, both measured on a Germany import (8 cores): each session raises
/// `maintenance_work_mem` + `max_parallel_maintenance_workers` (parallel sort / GiST build), and
/// the units build in parallel so the small tables hide under the dominant edge-table GiST.
pub async fn create_indexes(pool: &Pool, tables: &[&str], way_geom_tables: &[&str]) -> anyhow::Result<()> {
    // One build unit per tag table, plus the shared edge table.
    let mut units: Vec<Vec<String>> = tables.iter().map(|t| tag_index_stmts(t).to_vec()).collect();
    units.push(edge_index_stmts().to_vec());
    units.push(member_index_stmts().to_vec());
    units.push(node_index_stmts().to_vec());
    for table in way_geom_tables {
        units.push(way_geom_index_stmts(table).to_vec());
    }

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
