use deadpool_postgres::Pool;
use tokio_postgres::Client;

/// The single shared graph-edge table: one row per intersection-split sub-linestring of every kept
/// way (the extracted graph). Topic-independent, so every topic's tag table joins to this one table
/// on `osm_id`. Conceptually: one extracted graph, with the per-topic tag tables as disjoint
/// attribute layers over it. Created/populated only when some topic declares `"graph": { "way":
/// true }` (see `create_tables`'s `emit_graph` param) — otherwise no topic needs the routing graph
/// at all, so it's skipped entirely.
pub const EDGE_TABLE: &str = "edges";

/// Relation → member-way link table. Relations have no materialized geometry of their own; a
/// relation's tag row joins here to reach its member ways' rows in `EDGE_TABLE`, or a topic can
/// build its own relation geometry directly (see `relation_geom_table`/`main.rs`'s relation-geometry
/// step). Always created — a relation-member link is meaningful even when no topic wants the
/// routing graph.
pub const MEMBER_TABLE: &str = "relation_members";

/// Graph-vertex table: one row per node referenced as a `start_id`/`end_id` in `EDGE_TABLE` — shared
/// between ≥2 ways, a way endpoint, or forced by a node classifier. This is the
/// `edges.start_id`/`end_id` ↔ `osm_id` mapping, load-bearing (not optional) whenever `EDGE_TABLE`
/// exists — so it shares that same `emit_graph` gate; `id` is the internal sequential id `edges`
/// joins on, `osm_id` is the original OSM node id.
pub const NODE_TABLE: &str = "nodes";

/// This topic's node geometry table name (populated for topics declaring `"geometry_output": {
/// "node": "point" }` — always a `Point`, the only shape `GeometryOutputSpec` allows for nodes).
pub fn node_geom_table(table: &str) -> String {
    format!("{table}_node_geom")
}

/// This topic's way geometry table name (populated for topics declaring any `"geometry_output": {
/// "way": ... }`). One table per topic regardless of shape — `geom_type` distinguishes
/// Point/LineString/Polygon rows within it (see `geom::rows::GeomRow`'s own doc for why one table
/// per kind replaced one table per (kind, shape) pair).
pub fn way_geom_table(table: &str) -> String {
    format!("{table}_way_geom")
}

/// This topic's relation geometry table name (populated for topics declaring any
/// `"geometry_output": { "relation": ... }` — built in-process from member ways' independently
/// re-resolved coordinates, see `geom::relation` / `main.rs`'s post-stream step). `geom_type`
/// distinguishes Point/MultiLineString/Polygon rows within it, same as `way_geom_table`.
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
/// `emits_id` false drops the `id` column entirely (`IdType::None`); uniqueness then rests on
/// `(osm_type, osm_id)` instead — see `tag_index_stmts`.
fn create_tag_table_sql(table: &str, emits_id: bool) -> String {
    let id_col = if emits_id { "  id        text NOT NULL,\n" } else { "" };
    format!(r#"
CREATE TABLE IF NOT EXISTS {table} (
  osm_id    bigint,
  osm_type  text,
{id_col}  category  text,
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

fn create_node_table_sql() -> String {
    format!(r#"
CREATE TABLE IF NOT EXISTS {NODE_TABLE} (
  id     bigint,
  osm_id bigint,
  geom   geometry(Point, 3857)
)"#)
}

/// One per-kind geometry table: `(osm_id, geom_type, geom, length_m)`, one row per kept element
/// (node, way, or relation) declaring a geometry output for that kind — everything except the
/// always-on shared `edges`/`nodes` tables and the per-topic tag tables. The PostGIS column is
/// typed `geometry(Geometry, 3857)` (not a fixed subtype like `LineString`) since one table now
/// holds whichever shape(s) that kind's topics actually declared — `geom_type` (`'Point'`/
/// `'LineString'`/`'MultiLineString'`/`'Polygon'`) says which, mirroring `GeomRow`'s CSV/Parquet
/// shape. `length_m` is `NULL` for Point/Polygon rows — only a line has a length.
fn create_geom_table_sql(name: &str) -> String {
    format!(r#"
CREATE TABLE IF NOT EXISTS {name} (
  osm_id    bigint,
  geom_type text,
  geom      geometry(Geometry, 3857),
  length_m  double precision
)"#)
}

fn drop_geom_indexes_sql(name: &str) -> String {
    format!("DROP INDEX IF EXISTS {name}_geom_idx;\nDROP INDEX IF EXISTS {name}_osm_id_idx")
}

fn geom_index_stmts(name: &str) -> [String; 2] {
    [
        format!("CREATE INDEX IF NOT EXISTS {name}_geom_idx ON {name} USING GIST (geom)"),
        format!("CREATE INDEX IF NOT EXISTS {name}_osm_id_idx ON {name} (osm_id)"),
    ]
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

fn drop_node_indexes_sql() -> String {
    format!(
        "DROP INDEX IF EXISTS {NODE_TABLE}_geom_idx;\n\
         DROP INDEX IF EXISTS {NODE_TABLE}_osm_id_idx;\n\
         DROP INDEX IF EXISTS {NODE_TABLE}_id_idx"
    )
}

/// Tag-table indexes: unique feature id, join key on osm_id.
fn tag_index_stmts(table: &str, emits_id: bool) -> [String; 2] {
    [
        if emits_id {
            format!("CREATE UNIQUE INDEX IF NOT EXISTS {table}_id_idx ON {table} (id)")
        } else {
            // Without the `id` column, `(osm_type, osm_id)` is the row's identity — and it really is
            // unique there, since only a side-splitting topic emits several rows per `osm_id`, which
            // `IdType::None` refuses to coexist with (see `TopicRunner::load`).
            format!("CREATE UNIQUE INDEX IF NOT EXISTS {table}_id_idx ON {table} (osm_type, osm_id)")
        },
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

fn node_index_stmts() -> [String; 3] {
    [
        format!("CREATE INDEX IF NOT EXISTS {NODE_TABLE}_geom_idx ON {NODE_TABLE} USING GIST (geom)"),
        format!("CREATE INDEX IF NOT EXISTS {NODE_TABLE}_osm_id_idx ON {NODE_TABLE} (osm_id)"),
        format!("CREATE UNIQUE INDEX IF NOT EXISTS {NODE_TABLE}_id_idx ON {NODE_TABLE} (id)"),
    ]
}

/// `geom_tables`: every non-tag-table geometry table that should exist this run — one already-
/// fully-formatted table name per topic wanting node/way/relation geometry (see
/// `node_geom_table`/`way_geom_table`/`relation_geom_table`). Threaded through `create_tables`/
/// `truncate_tables`/`drop_indexes`/`create_indexes` alongside the always-present `EDGE_TABLE` +
/// `MEMBER_TABLE` + `NODE_TABLE`.
/// `emit_graph`: whether any topic actually declares `"graph": { "way": true }` — the shared
/// `EDGE_TABLE`/`NODE_TABLE` pair exists solely to back that (`start_id`/`end_id`, the extracted
/// routing graph), so when nothing wants it, neither table is created/touched at all, same
/// principle as the per-topic `geom_tables`.
pub async fn create_tables(
    client: &Client,
    tables: &[(&str, bool)],
    geom_tables: &[String],
    emit_graph: bool,
) -> anyhow::Result<()> {
    for &(table, emits_id) in tables {
        client.batch_execute(&create_tag_table_sql(table, emits_id)).await?;
    }
    if emit_graph {
        client.batch_execute(&create_edge_table_sql()).await?;
        client.batch_execute(&create_node_table_sql()).await?;
    }
    client.batch_execute(&create_member_table_sql()).await?;
    for name in geom_tables {
        client.batch_execute(&create_geom_table_sql(name)).await?;
    }
    Ok(())
}

pub async fn truncate_tables(
    client: &Client,
    tables: &[&str],
    geom_tables: &[String],
    emit_graph: bool,
) -> anyhow::Result<()> {
    let mut all: Vec<String> = tables.iter().map(|t| t.to_string()).collect();
    if emit_graph {
        all.push(EDGE_TABLE.to_string());
        all.push(NODE_TABLE.to_string());
    }
    all.push(MEMBER_TABLE.to_string());
    all.extend(geom_tables.iter().cloned());
    client.batch_execute(&format!("TRUNCATE TABLE {}", all.join(", "))).await?;
    Ok(())
}

pub async fn drop_indexes(
    client: &Client,
    tables: &[&str],
    geom_tables: &[String],
    emit_graph: bool,
) -> anyhow::Result<()> {
    for table in tables {
        client.batch_execute(&drop_tag_indexes_sql(table)).await?;
    }
    if emit_graph {
        client.batch_execute(&drop_edge_indexes_sql()).await?;
        client.batch_execute(&drop_node_indexes_sql()).await?;
    }
    client.batch_execute(&drop_member_indexes_sql()).await?;
    for name in geom_tables {
        client.batch_execute(&drop_geom_indexes_sql(name)).await?;
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
pub async fn create_indexes(
    pool: &Pool,
    tables: &[(&str, bool)],
    geom_tables: &[String],
    emit_graph: bool,
) -> anyhow::Result<()> {
    // One build unit per tag table, plus the shared edge table.
    let mut units: Vec<Vec<String>> =
        tables.iter().map(|&(t, emits_id)| tag_index_stmts(t, emits_id).to_vec()).collect();
    if emit_graph {
        units.push(edge_index_stmts().to_vec());
        units.push(node_index_stmts().to_vec());
    }
    units.push(member_index_stmts().to_vec());
    for name in geom_tables {
        units.push(geom_index_stmts(name).to_vec());
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
