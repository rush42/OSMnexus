use tokio_postgres::Client;

fn create_table_sql(table: &str) -> String {
    format!(r#"
CREATE TABLE IF NOT EXISTS {table} (
  osm_id    bigint,
  osm_type  text,
  id        text NOT NULL,
  osm       jsonb,
  sanitized jsonb,
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

fn create_indexes_sql(table: &str) -> String {
    format!(
        "CREATE INDEX IF NOT EXISTS {table}_geom_idx    ON {table} USING GIST (geom);\n\
         CREATE INDEX IF NOT EXISTS {table}_minzoom_idx ON {table} (minzoom);\n\
         CREATE UNIQUE INDEX IF NOT EXISTS {table}_id_idx ON {table} (id)"
    )
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

pub async fn create_indexes(client: &Client, tables: &[&str]) -> anyhow::Result<()> {
    for table in tables {
        client.batch_execute(&create_indexes_sql(table)).await?;
    }
    Ok(())
}
