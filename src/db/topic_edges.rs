//! Materializes per-topic, pgRouting-shaped edge tables from a topic's `cost`/`is_directed`
//! fields, projected onto the shared `EDGE_TABLE`. See `config::TopicEdgeMode`.

use tokio_postgres::Client;

use crate::config::TopicEdgeMode;

use super::schema::EDGE_TABLE;

/// Static SQL template — `table` is always one of the Rust-provided topic table identifiers
/// already trusted throughout `schema.rs` (never user/OSM-derived), so this is not an
/// injection surface. The two jsonb keys read (`cost`, `is_directed`) are fixed literals, read
/// from `derived` (every topic output — see `TopicSpec::outputs` — lands there).
/// `cost` is split proportionally by segment share (`length_m / total_length_m`) so a way
/// split into several edge segments doesn't get an unfair per-meter discount.
/// `reverse_cost = -1` follows the pgRouting convention for "unusable in that direction".
/// `mode == All` additionally joins in the topic's own tag columns.
fn create_topic_edge_table_sql(table: &str, mode: TopicEdgeMode) -> String {
    let extra_columns = match mode {
        TopicEdgeMode::Pgrouting => "",
        TopicEdgeMode::All => ", t.osm_type, t.id, t.derived, t.private, t.meta",
    };
    format!(
        r#"
DROP TABLE IF EXISTS {table}_edge;
CREATE TABLE {table}_edge AS
SELECT e.osm_id, e.seg_idx, e.start_id, e.end_id, e.geom, e.length_m, e.total_length_m,
       (t.derived->>'cost')::double precision
         * (e.length_m / NULLIF(e.total_length_m, 0)) AS cost,
       CASE WHEN COALESCE((t.derived->>'is_directed')::boolean, false) THEN -1
            ELSE (t.derived->>'cost')::double precision
                 * (e.length_m / NULLIF(e.total_length_m, 0))
       END AS reverse_cost{extra_columns}
FROM {EDGE_TABLE} e
JOIN {table} t ON t.osm_id = e.osm_id
WHERE t.derived ? 'cost'
"#
    )
}

fn topic_edge_index_stmts(table: &str) -> [String; 2] {
    [
        format!("CREATE INDEX ON {table}_edge USING GIST (geom)"),
        format!("CREATE INDEX ON {table}_edge (osm_id)"),
    ]
}

pub async fn materialize(
    client: &Client,
    table: &str,
    mode: TopicEdgeMode,
    create_index: bool,
) -> anyhow::Result<()> {
    client.batch_execute(&create_topic_edge_table_sql(table, mode)).await?;
    if create_index {
        for stmt in topic_edge_index_stmts(table) {
            client.batch_execute(&stmt).await?;
        }
    }
    Ok(())
}
