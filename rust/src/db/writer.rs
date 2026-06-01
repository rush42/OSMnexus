use bytes::Bytes;
use futures::SinkExt;
use tokio_postgres::Client;

use crate::output::{bikelane_row::BikelaneRow, geometry::to_ewkb, road_row::RoadRow};

pub const COPY_BIKELANES: &str =
    "COPY bikelanes (osm_id, osm_type, id, osm, sanitized, derived, private, meta, geom, minzoom) FROM STDIN (FORMAT CSV)";

pub const COPY_ROADS: &str =
    "COPY roads (osm_id, osm_type, id, osm, sanitized, derived, private, meta, geom, minzoom) FROM STDIN (FORMAT CSV)";

/// Write all bikelane rows using PostgreSQL COPY CSV.
pub async fn write_bikelanes(client: &Client, rows: &[BikelaneRow]) -> anyhow::Result<usize> {
    let sink = client.copy_in(COPY_BIKELANES).await?;
    let mut sink = std::pin::pin!(sink);
    let mut buf = Vec::new();
    for row in rows {
        write_bikelane_csv_row(&mut buf, row)?;
    }
    sink.send(Bytes::from(buf)).await?;
    sink.finish().await?;
    Ok(rows.len())
}

/// Write all road rows using PostgreSQL COPY CSV.
pub async fn write_roads(client: &Client, rows: &[RoadRow]) -> anyhow::Result<usize> {
    let sink = client.copy_in(COPY_ROADS).await?;
    let mut sink = std::pin::pin!(sink);
    let mut buf = Vec::new();
    for row in rows {
        write_road_csv_row(&mut buf, row)?;
    }
    sink.send(Bytes::from(buf)).await?;
    sink.finish().await?;
    Ok(rows.len())
}

pub fn write_bikelane_csv_row(buf: &mut Vec<u8>, row: &BikelaneRow) -> anyhow::Result<()> {
    let osm_json       = serde_json::to_string(&row.osm)?;
    let sanitized_json = serde_json::to_string(&row.sanitized)?;
    let derived_json   = serde_json::to_string(&row.derived)?;
    let private_json   = serde_json::to_string(&row.private)?;
    let meta_json      = serde_json::to_string(&row.meta)?;
    let ewkb_hex       = hex::encode(to_ewkb(&row.geom));

    write_csv_row(buf, &[
        row.osm_id.to_string(),
        row.osm_type.to_owned(),
        row.id.clone(),
        osm_json,
        sanitized_json,
        derived_json,
        private_json,
        meta_json,
        ewkb_hex,
        row.minzoom.to_string(),
    ]);
    Ok(())
}

pub fn write_road_csv_row(buf: &mut Vec<u8>, row: &RoadRow) -> anyhow::Result<()> {
    let osm_json       = serde_json::to_string(&row.osm)?;
    let sanitized_json = serde_json::to_string(&row.sanitized)?;
    let derived_json   = serde_json::to_string(&row.derived)?;
    let private_json   = serde_json::to_string(&row.private)?;
    let meta_json      = serde_json::to_string(&row.meta)?;
    let ewkb_hex       = hex::encode(to_ewkb(&row.geom));

    write_csv_row(buf, &[
        row.osm_id.to_string(),
        row.osm_type.to_owned(),
        row.id.clone(),
        osm_json,
        sanitized_json,
        derived_json,
        private_json,
        meta_json,
        ewkb_hex,
        row.minzoom.to_string(),
    ]);
    Ok(())
}

/// Write a single CSV row with RFC 4180 quoting.
fn write_csv_row(buf: &mut Vec<u8>, fields: &[String]) {
    for (i, field) in fields.iter().enumerate() {
        if i > 0 { buf.push(b','); }
        let needs_quoting =
            field.contains('"') || field.contains(',') || field.contains('\n') || field.contains('\\');
        if needs_quoting {
            buf.push(b'"');
            for ch in field.chars() {
                if ch == '"' {
                    buf.extend_from_slice(b"\"\"");
                } else {
                    let mut tmp = [0u8; 4];
                    buf.extend_from_slice(ch.encode_utf8(&mut tmp).as_bytes());
                }
            }
            buf.push(b'"');
        } else {
            buf.extend_from_slice(field.as_bytes());
        }
    }
    buf.push(b'\n');
}
