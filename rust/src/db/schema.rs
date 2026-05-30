use tokio_postgres::Client;

const CREATE_BIKELANES: &str = r#"
CREATE TABLE IF NOT EXISTS bikelanes (
  osm_id   bigint,
  osm_type text,
  id       text NOT NULL,
  osm      jsonb,
  derived  jsonb,
  meta     jsonb,
  geom     geometry(LineString, 3857),
  minzoom  integer NOT NULL
)"#;

const CREATE_ROADS: &str = r#"
CREATE TABLE IF NOT EXISTS roads (
  osm_id   bigint,
  osm_type text,
  id       text NOT NULL,
  osm      jsonb,
  derived  jsonb,
  meta     jsonb,
  geom     geometry(LineString, 3857),
  minzoom  integer NOT NULL
)"#;

const DROP_BIKELANE_INDEXES: &str = r#"
DROP INDEX IF EXISTS bikelanes_geom_idx;
DROP INDEX IF EXISTS bikelanes_minzoom_idx;
DROP INDEX IF EXISTS bikelanes_id_idx
"#;

const DROP_ROAD_INDEXES: &str = r#"
DROP INDEX IF EXISTS roads_geom_idx;
DROP INDEX IF EXISTS roads_minzoom_idx;
DROP INDEX IF EXISTS roads_id_idx
"#;

const CREATE_BIKELANE_INDEXES: &str = r#"
CREATE INDEX IF NOT EXISTS bikelanes_geom_idx    ON bikelanes USING GIST (geom);
CREATE INDEX IF NOT EXISTS bikelanes_minzoom_idx ON bikelanes (minzoom);
CREATE UNIQUE INDEX IF NOT EXISTS bikelanes_id_idx ON bikelanes (id)
"#;

const CREATE_ROAD_INDEXES: &str = r#"
CREATE INDEX IF NOT EXISTS roads_geom_idx    ON roads USING GIST (geom);
CREATE INDEX IF NOT EXISTS roads_minzoom_idx ON roads (minzoom);
CREATE UNIQUE INDEX IF NOT EXISTS roads_id_idx ON roads (id)
"#;

pub async fn create_tables(client: &Client) -> anyhow::Result<()> {
    client.batch_execute(CREATE_BIKELANES).await?;
    client.batch_execute(CREATE_ROADS).await?;
    Ok(())
}

pub async fn truncate_tables(client: &Client) -> anyhow::Result<()> {
    client
        .batch_execute("TRUNCATE TABLE bikelanes, roads")
        .await?;
    Ok(())
}

pub async fn drop_indexes(client: &Client) -> anyhow::Result<()> {
    client.batch_execute(DROP_BIKELANE_INDEXES).await?;
    client.batch_execute(DROP_ROAD_INDEXES).await?;
    Ok(())
}

pub async fn create_indexes(client: &Client) -> anyhow::Result<()> {
    client.batch_execute(CREATE_BIKELANE_INDEXES).await?;
    client.batch_execute(CREATE_ROAD_INDEXES).await?;
    Ok(())
}
