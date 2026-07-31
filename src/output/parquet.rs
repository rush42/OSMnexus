//! Builds one `{table}.parquet` file per topic from the staged CSV output `--output parquet`
//! shares with `--output csv`: `{table}.csv` for classified tag rows plus the topic's paired
//! geometry table(s). This keeps the streaming writer path unchanged and does one bounded
//! post-processing pass over the emitted CSV files to assemble a self-contained analytical table.

use std::collections::{BTreeSet, HashMap};
use std::fs::File;
use std::io::Write;
use std::path::Path;
use std::sync::Arc;

use anyhow::Context;
use arrow_array::builder::{BinaryBuilder, Float64Builder, Int64Builder, StringBuilder};
use arrow_array::{ArrayRef, RecordBatch};
use arrow_schema::{DataType, Field, Schema};
use parquet::basic::Compression;
use parquet::arrow::ArrowWriter;
use parquet::file::properties::WriterProperties;
use parquet::format::KeyValue;
use serde_json::json;

use crate::geom::primitives::{linestring_from_ewkb, mercator_to_wgs84, point_from_ewkb};
use crate::geom::rows::{EDGE_COLUMNS, POINT_COLUMNS, POLYGON_COLUMNS, WAY_COLUMNS};
use crate::output::rows::{MEMBER_COLUMNS, TAG_COLUMNS};

#[derive(Clone)]
struct GeomRow {
    geom_type: &'static str,
    geometry: Vec<u8>,
    length_m: Option<f64>,
    seg_idx: Option<i64>,
    start_id: Option<i64>,
    end_id: Option<i64>,
    total_length_m: Option<f64>,
    cost: Option<f64>,
    reverse_cost: Option<f64>,
}

fn wkb_point(lon: f64, lat: f64) -> Vec<u8> {
    let mut buf = Vec::with_capacity(1 + 4 + 16);
    buf.write_all(&[1u8]).unwrap();
    buf.write_all(&1u32.to_le_bytes()).unwrap();
    buf.write_all(&lon.to_le_bytes()).unwrap();
    buf.write_all(&lat.to_le_bytes()).unwrap();
    buf
}

fn wkb_linestring(coords: &[(f64, f64)]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(1 + 4 + 4 + 16 * coords.len());
    buf.write_all(&[1u8]).unwrap();
    buf.write_all(&2u32.to_le_bytes()).unwrap();
    buf.write_all(&(coords.len() as u32).to_le_bytes()).unwrap();
    for (lon, lat) in coords {
        buf.write_all(&lon.to_le_bytes()).unwrap();
        buf.write_all(&lat.to_le_bytes()).unwrap();
    }
    buf
}

fn wkb_polygon(rings: &[Vec<(f64, f64)>]) -> Vec<u8> {
    let total_points: usize = rings.iter().map(Vec::len).sum();
    let mut buf = Vec::with_capacity(1 + 4 + 4 + rings.len() * 4 + total_points * 16);
    buf.write_all(&[1u8]).unwrap();
    buf.write_all(&3u32.to_le_bytes()).unwrap();
    buf.write_all(&(rings.len() as u32).to_le_bytes()).unwrap();
    for ring in rings {
        buf.write_all(&(ring.len() as u32).to_le_bytes()).unwrap();
        for (lon, lat) in ring {
            buf.write_all(&lon.to_le_bytes()).unwrap();
            buf.write_all(&lat.to_le_bytes()).unwrap();
        }
    }
    buf
}

fn wkb_linestring_lonlat_from_ewkb(bytes: &[u8]) -> anyhow::Result<Vec<u8>> {
    let coords = linestring_from_ewkb(bytes)?;
    let lonlat: Vec<(f64, f64)> = coords
        .into_iter()
        .map(|(x, y)| mercator_to_wgs84(x, y))
        .collect();
    Ok(wkb_linestring(&lonlat))
}

fn wkb_point_lonlat_from_ewkb(bytes: &[u8]) -> anyhow::Result<Vec<u8>> {
    let (x, y) = point_from_ewkb(bytes)?;
    let (lon, lat) = mercator_to_wgs84(x, y);
    Ok(wkb_point(lon, lat))
}

fn polygon_rings_from_ewkb(bytes: &[u8]) -> anyhow::Result<Vec<Vec<(f64, f64)>>> {
    anyhow::ensure!(bytes.len() >= 13, "EWKB too short for a Polygon header");
    anyhow::ensure!(bytes[0] == 1, "only little-endian EWKB is supported");
    let wkb_type = u32::from_le_bytes(bytes[1..5].try_into().unwrap());
    anyhow::ensure!(wkb_type == 0x2000_0003, "expected SRID-flagged Polygon, got type {wkb_type:#x}");
    let num_rings = u32::from_le_bytes(bytes[9..13].try_into().unwrap()) as usize;
    let mut off = 13;
    let mut rings = Vec::with_capacity(num_rings);
    for _ in 0..num_rings {
        anyhow::ensure!(bytes.len() >= off + 4, "EWKB truncated before polygon ring size");
        let num_points = u32::from_le_bytes(bytes[off..off + 4].try_into().unwrap()) as usize;
        off += 4;
        anyhow::ensure!(bytes.len() >= off + num_points * 16, "EWKB truncated inside polygon ring");
        let mut ring = Vec::with_capacity(num_points);
        for _ in 0..num_points {
            let x = f64::from_le_bytes(bytes[off..off + 8].try_into().unwrap());
            let y = f64::from_le_bytes(bytes[off + 8..off + 16].try_into().unwrap());
            ring.push(mercator_to_wgs84(x, y));
            off += 16;
        }
        rings.push(ring);
    }
    Ok(rings)
}

fn wkb_polygon_lonlat_from_ewkb(bytes: &[u8]) -> anyhow::Result<Vec<u8>> {
    Ok(wkb_polygon(&polygon_rings_from_ewkb(bytes)?))
}

fn geoparquet_crs84() -> serde_json::Value {
    json!({
        "$schema": "https://proj.org/schemas/v0.5/projjson.schema.json",
        "type": "GeographicCRS",
        "name": "WGS 84 longitude-latitude",
        "datum": {
            "type": "GeodeticReferenceFrame",
            "name": "World Geodetic System 1984",
            "ellipsoid": {
                "name": "WGS 84",
                "semi_major_axis": 6378137.0,
                "inverse_flattening": 298.257223563
            }
        },
        "coordinate_system": {
            "subtype": "ellipsoidal",
            "axis": [
                {
                    "name": "Geodetic longitude",
                    "abbreviation": "Lon",
                    "direction": "east",
                    "unit": "degree"
                },
                {
                    "name": "Geodetic latitude",
                    "abbreviation": "Lat",
                    "direction": "north",
                    "unit": "degree"
                }
            ]
        },
        "id": {
            "authority": "OGC",
            "code": "CRS84"
        }
    })
}

fn geo_metadata_json(geometry_types: Vec<&str>) -> String {
    json!({
        "version": "1.1.0",
        "primary_column": "geometry",
        "columns": {
            "geometry": {
                "encoding": "WKB",
                "geometry_types": geometry_types,
                "crs": geoparquet_crs84()
            }
        }
    })
    .to_string()
}

fn geo_key_value(geo_json: &str) -> KeyValue {
    KeyValue {
        key: "geo".to_owned(),
        value: Some(geo_json.to_owned()),
    }
}

fn writer_props(geo_json: &str) -> WriterProperties {
    WriterProperties::builder()
        .set_compression(Compression::UNCOMPRESSED)
        .set_key_value_metadata(Some(vec![geo_key_value(geo_json)]))
        .build()
}

fn read_relation_members(path: &Path) -> anyhow::Result<HashMap<i64, Vec<i64>>> {
    debug_assert_eq!(MEMBER_COLUMNS, "relation_osm_id,way_osm_id");
    let mut reader = csv::Reader::from_path(path)?;
    let mut by_relation_id: HashMap<i64, Vec<i64>> = HashMap::new();
    for result in reader.records() {
        let record = result?;
        let relation_osm_id: i64 = record[0].parse()?;
        let way_osm_id: i64 = record[1].parse()?;
        by_relation_id.entry(relation_osm_id).or_default().push(way_osm_id);
    }
    Ok(by_relation_id)
}

fn read_edges(path: &Path) -> anyhow::Result<HashMap<i64, Vec<GeomRow>>> {
    debug_assert_eq!(EDGE_COLUMNS, "osm_id,seg_idx,start_id,end_id,geom,length_m,total_length_m,cost,reverse_cost");
    let mut reader = csv::Reader::from_path(path)?;
    let mut by_osm_id: HashMap<i64, Vec<GeomRow>> = HashMap::new();
    for result in reader.records() {
        let record = result?;
        let geom_hex = &record[4];
        if geom_hex.is_empty() {
            continue;
        }
        let osm_id: i64 = record[0].parse()?;
        by_osm_id.entry(osm_id).or_default().push(GeomRow {
            geom_type: "LineString",
            geometry: wkb_linestring_lonlat_from_ewkb(&hex::decode(geom_hex)?)?,
            length_m: Some(record[5].parse()?),
            seg_idx: Some(record[1].parse()?),
            start_id: Some(record[2].parse()?),
            end_id: Some(record[3].parse()?),
            total_length_m: Some(record[6].parse()?),
            cost: Some(record[7].parse()?),
            reverse_cost: Some(record[8].parse()?),
        });
    }
    for segments in by_osm_id.values_mut() {
        segments.sort_by_key(|segment| segment.seg_idx.unwrap_or_default());
    }
    Ok(by_osm_id)
}

fn read_way_geom(path: &Path) -> anyhow::Result<Option<HashMap<i64, GeomRow>>> {
    if !path.exists() {
        return Ok(None);
    }
    debug_assert_eq!(WAY_COLUMNS, "osm_id,geom,length_m");
    let mut reader = csv::Reader::from_path(path)?;
    let mut by_osm_id = HashMap::new();
    for result in reader.records() {
        let record = result?;
        let geom_hex = &record[1];
        if geom_hex.is_empty() {
            continue;
        }
        let osm_id: i64 = record[0].parse()?;
        by_osm_id.insert(
            osm_id,
            GeomRow {
                geom_type: "LineString",
                geometry: wkb_linestring_lonlat_from_ewkb(&hex::decode(geom_hex)?)?,
                length_m: Some(record[2].parse()?),
                seg_idx: None,
                start_id: None,
                end_id: None,
                total_length_m: None,
                cost: None,
                reverse_cost: None,
            },
        );
    }
    Ok(Some(by_osm_id))
}

fn read_point_geom(path: &Path) -> anyhow::Result<Option<HashMap<i64, GeomRow>>> {
    if !path.exists() {
        return Ok(None);
    }
    debug_assert_eq!(POINT_COLUMNS, "osm_id,geom");
    let mut reader = csv::Reader::from_path(path)?;
    let mut by_osm_id = HashMap::new();
    for result in reader.records() {
        let record = result?;
        let geom_hex = &record[1];
        if geom_hex.is_empty() {
            continue;
        }
        let osm_id: i64 = record[0].parse()?;
        by_osm_id.insert(
            osm_id,
            GeomRow {
                geom_type: "Point",
                geometry: wkb_point_lonlat_from_ewkb(&hex::decode(geom_hex)?)?,
                length_m: None,
                seg_idx: None,
                start_id: None,
                end_id: None,
                total_length_m: None,
                cost: None,
                reverse_cost: None,
            },
        );
    }
    Ok(Some(by_osm_id))
}

fn read_polygon_geom(path: &Path) -> anyhow::Result<Option<HashMap<i64, GeomRow>>> {
    if !path.exists() {
        return Ok(None);
    }
    debug_assert_eq!(POLYGON_COLUMNS, "osm_id,geom");
    let mut reader = csv::Reader::from_path(path)?;
    let mut by_osm_id = HashMap::new();
    for result in reader.records() {
        let record = result?;
        let geom_hex = &record[1];
        if geom_hex.is_empty() {
            continue;
        }
        let osm_id: i64 = record[0].parse()?;
        by_osm_id.insert(
            osm_id,
            GeomRow {
                geom_type: "Polygon",
                geometry: wkb_polygon_lonlat_from_ewkb(&hex::decode(geom_hex)?)?,
                length_m: None,
                seg_idx: None,
                start_id: None,
                end_id: None,
                total_length_m: None,
                cost: None,
                reverse_cost: None,
            },
        );
    }
    Ok(Some(by_osm_id))
}

fn append_optional_str(builder: &mut StringBuilder, value: Option<&str>) {
    if let Some(value) = value {
        builder.append_value(value);
    } else {
        builder.append_null();
    }
}

fn append_optional_i64(builder: &mut Int64Builder, value: Option<i64>) {
    if let Some(value) = value {
        builder.append_value(value);
    } else {
        builder.append_null();
    }
}

fn append_optional_f64(builder: &mut Float64Builder, value: Option<f64>) {
    if let Some(value) = value {
        builder.append_value(value);
    } else {
        builder.append_null();
    }
}

fn write_table_parquet(
    out_dir: &Path,
    table: &str,
    edges: &HashMap<i64, Vec<GeomRow>>,
    relation_members: &HashMap<i64, Vec<i64>>,
) -> anyhow::Result<()> {
    debug_assert_eq!(TAG_COLUMNS, "osm_id,osm_type,id,category,produced,annotations,meta");

    let way_geom = read_way_geom(&out_dir.join(format!("{table}_geom.csv")))?;
    let way_point = read_point_geom(&out_dir.join(format!("{table}_point.csv")))?;
    let way_polygon = read_polygon_geom(&out_dir.join(format!("{table}_polygon.csv")))?;
    let relation_geom = read_way_geom(&out_dir.join(format!("{table}_relation_geom.csv")))?;
    let relation_point = read_point_geom(&out_dir.join(format!("{table}_relation_point.csv")))?;
    let relation_polygon = read_polygon_geom(&out_dir.join(format!("{table}_relation_polygon.csv")))?;

    let mut osm_id_builder = Int64Builder::new();
    let mut osm_type_builder = StringBuilder::new();
    let mut id_builder = StringBuilder::new();
    let mut category_builder = StringBuilder::new();
    let mut produced_builder = StringBuilder::new();
    let mut annotations_builder = StringBuilder::new();
    let mut meta_builder = StringBuilder::new();
    let mut geom_type_builder = StringBuilder::new();
    let mut geometry_builder = BinaryBuilder::new();
    let mut length_builder = Float64Builder::new();
    let mut seg_idx_builder = Int64Builder::new();
    let mut start_id_builder = Int64Builder::new();
    let mut end_id_builder = Int64Builder::new();
    let mut total_length_builder = Float64Builder::new();
    let mut cost_builder = Float64Builder::new();
    let mut reverse_cost_builder = Float64Builder::new();
    let mut geometry_types = BTreeSet::new();

    let mut reader = csv::Reader::from_path(out_dir.join(format!("{table}.csv")))
        .with_context(|| format!("opening {table}.csv for parquet export"))?;

    for result in reader.records() {
        let record = result?;
        let osm_id: i64 = record[0].parse()?;
        let osm_type = &record[1];

        let mut geom_rows: Vec<&GeomRow> = Vec::new();
        match osm_type {
            "W" => {
                if let Some(geom) = way_geom.as_ref().and_then(|m| m.get(&osm_id)) {
                    geom_rows.push(geom);
                } else if let Some(point) = way_point.as_ref().and_then(|m| m.get(&osm_id)) {
                    geom_rows.push(point);
                } else if let Some(polygon) = way_polygon.as_ref().and_then(|m| m.get(&osm_id)) {
                    geom_rows.push(polygon);
                } else if let Some(segments) = edges.get(&osm_id) {
                    geom_rows.extend(segments.iter());
                }
            }
            "N" => {
                if let Some(point) = way_point.as_ref().and_then(|m| m.get(&osm_id)) {
                    geom_rows.push(point);
                }
            }
            "R" => {
                if let Some(geom) = relation_geom.as_ref().and_then(|m| m.get(&osm_id)) {
                    geom_rows.push(geom);
                } else if let Some(point) = relation_point.as_ref().and_then(|m| m.get(&osm_id)) {
                    geom_rows.push(point);
                } else if let Some(polygon) = relation_polygon.as_ref().and_then(|m| m.get(&osm_id)) {
                    geom_rows.push(polygon);
                } else if let Some(way_ids) = relation_members.get(&osm_id) {
                    for way_id in way_ids {
                        if let Some(segments) = edges.get(way_id) {
                            geom_rows.extend(segments.iter());
                        }
                    }
                }
            }
            _ => {}
        }

        if geom_rows.is_empty() {
            osm_id_builder.append_value(osm_id);
            osm_type_builder.append_value(osm_type);
            id_builder.append_value(&record[2]);
            append_optional_str(
                &mut category_builder,
                (!record[3].is_empty()).then_some(record[3].as_ref()),
            );
            produced_builder.append_value(&record[4]);
            annotations_builder.append_value(&record[5]);
            meta_builder.append_value(&record[6]);
            geom_type_builder.append_null();
            geometry_builder.append_null();
            length_builder.append_null();
            seg_idx_builder.append_null();
            start_id_builder.append_null();
            end_id_builder.append_null();
            total_length_builder.append_null();
            cost_builder.append_null();
            reverse_cost_builder.append_null();
            continue;
        }

        for geom_row in geom_rows {
            osm_id_builder.append_value(osm_id);
            osm_type_builder.append_value(osm_type);
            id_builder.append_value(&record[2]);
            append_optional_str(
                &mut category_builder,
                (!record[3].is_empty()).then_some(record[3].as_ref()),
            );
            produced_builder.append_value(&record[4]);
            annotations_builder.append_value(&record[5]);
            meta_builder.append_value(&record[6]);
            geom_type_builder.append_value(geom_row.geom_type);
            geometry_builder.append_value(&geom_row.geometry);
            append_optional_f64(&mut length_builder, geom_row.length_m);
            append_optional_i64(&mut seg_idx_builder, geom_row.seg_idx);
            append_optional_i64(&mut start_id_builder, geom_row.start_id);
            append_optional_i64(&mut end_id_builder, geom_row.end_id);
            append_optional_f64(&mut total_length_builder, geom_row.total_length_m);
            append_optional_f64(&mut cost_builder, geom_row.cost);
            append_optional_f64(&mut reverse_cost_builder, geom_row.reverse_cost);
            geometry_types.insert(geom_row.geom_type);
        }
    }

    let geo_metadata = geo_metadata_json(geometry_types.into_iter().collect::<Vec<_>>());

    let mut schema_metadata = HashMap::new();
    schema_metadata.insert("geo".to_owned(), geo_metadata.clone());

    let schema = Arc::new(Schema::new(vec![
        Field::new("osm_id", DataType::Int64, false),
        Field::new("osm_type", DataType::Utf8, false),
        Field::new("id", DataType::Utf8, false),
        Field::new("category", DataType::Utf8, true),
        Field::new("produced", DataType::Utf8, false),
        Field::new("annotations", DataType::Utf8, false),
        Field::new("meta", DataType::Utf8, false),
        Field::new("geom_type", DataType::Utf8, true),
        Field::new("geometry", DataType::Binary, true),
        Field::new("length_m", DataType::Float64, true),
        Field::new("seg_idx", DataType::Int64, true),
        Field::new("start_id", DataType::Int64, true),
        Field::new("end_id", DataType::Int64, true),
        Field::new("total_length_m", DataType::Float64, true),
        Field::new("cost", DataType::Float64, true),
        Field::new("reverse_cost", DataType::Float64, true),
    ]).with_metadata(schema_metadata));

    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(osm_id_builder.finish()) as ArrayRef,
            Arc::new(osm_type_builder.finish()) as ArrayRef,
            Arc::new(id_builder.finish()) as ArrayRef,
            Arc::new(category_builder.finish()) as ArrayRef,
            Arc::new(produced_builder.finish()) as ArrayRef,
            Arc::new(annotations_builder.finish()) as ArrayRef,
            Arc::new(meta_builder.finish()) as ArrayRef,
            Arc::new(geom_type_builder.finish()) as ArrayRef,
            Arc::new(geometry_builder.finish()) as ArrayRef,
            Arc::new(length_builder.finish()) as ArrayRef,
            Arc::new(seg_idx_builder.finish()) as ArrayRef,
            Arc::new(start_id_builder.finish()) as ArrayRef,
            Arc::new(end_id_builder.finish()) as ArrayRef,
            Arc::new(total_length_builder.finish()) as ArrayRef,
            Arc::new(cost_builder.finish()) as ArrayRef,
            Arc::new(reverse_cost_builder.finish()) as ArrayRef,
        ],
    )?;

    let path = out_dir.join(format!("{table}.parquet"));
    let file = File::create(&path).with_context(|| format!("creating {}", path.display()))?;
    let mut writer = ArrowWriter::try_new(file, schema, Some(writer_props(&geo_metadata)))?;
    writer.write(&batch)?;
    writer.close()?;
    Ok(())
}

/// Reads the staged CSV output for each topic and writes one `{table}.parquet` file beside it.
/// Tag rows stay at their current granularity; topics backed by graph edges duplicate a tag row
/// per split edge segment so each parquet row still carries exactly one geometry payload.
pub fn write_parquet_from_csv(out_dir: &Path, tables: &[String]) -> anyhow::Result<()> {
    let edges_path = out_dir.join("edges.csv");
    let edges = if edges_path.exists() { read_edges(&edges_path)? } else { HashMap::new() };

    let members_path = out_dir.join("relation_members.csv");
    let relation_members = if members_path.exists() {
        read_relation_members(&members_path)?
    } else {
        HashMap::new()
    };

    for table in tables {
        write_table_parquet(out_dir, table, &edges, &relation_members)?;
    }
    Ok(())
}