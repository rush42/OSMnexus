//! Builds one `<table>.geojson` FeatureCollection per topic from the CSV output `--output geojson`
//! shares with `--output csv` (`{table}.csv` + `edges.csv`). Joins tag rows to edge geometries on
//! `osm_id`, mirroring the tile-materialization join that Postgres output defers to query time —
//! for local tooling (e.g. the live editor) that wants one self-contained file instead of a table
//! pair. A single OSM way can be split into several edge rows (one per `seg_idx`) at intersections;
//! the node shared between consecutive segments of the same way is surfaced as a "cut point" so
//! callers can see exactly where the graph broke a way apart.

use std::collections::HashMap;
use std::path::Path;

use serde_json::{json, Map, Value};

use crate::output::geometry::{linestring_from_ewkb, mercator_to_wgs84, point_from_ewkb};
use crate::output::rows::{GEOM_COLUMNS, NODE_COLUMNS, TAG_COLUMNS};

struct EdgeGeom {
    seg_idx: usize,
    // Decoded once in `read_edges`, not per feature — a way shared by many relations (e.g. a
    // trunk road several bus routes run down) would otherwise get its WKB re-parsed and
    // re-projected once per relation that references it.
    coordinates: Vec<[f64; 2]>,
}

fn lonlat_coordinates(geom: &[u8]) -> anyhow::Result<Vec<[f64; 2]>> {
    Ok(linestring_from_ewkb(geom)?
        .into_iter()
        .map(|(x, y)| {
            let (lon, lat) = mercator_to_wgs84(x, y);
            [lon, lat]
        })
        .collect())
}

fn lonlat_point(geom: &[u8]) -> anyhow::Result<[f64; 2]> {
    let (x, y) = point_from_ewkb(geom)?;
    let (lon, lat) = mercator_to_wgs84(x, y);
    Ok([lon, lat])
}

/// Reads `nodes.csv` (`id,osm_id,geom`), keyed by `osm_id`.
fn read_nodes(path: &Path) -> anyhow::Result<HashMap<i64, Vec<u8>>> {
    debug_assert_eq!(NODE_COLUMNS, "id,osm_id,geom");
    let mut reader = csv::Reader::from_path(path)?;
    let mut by_osm_id = HashMap::new();
    for result in reader.records() {
        let record = result?;
        let geom_hex = &record[2];
        if geom_hex.is_empty() {
            continue;
        }
        let osm_id: i64 = record[1].parse()?;
        by_osm_id.insert(osm_id, hex::decode(geom_hex)?);
    }
    Ok(by_osm_id)
}

/// Reads `relation_members.csv` (`relation_osm_id,way_osm_id`), keyed by `relation_osm_id`.
fn read_relation_members(path: &Path) -> anyhow::Result<HashMap<i64, Vec<i64>>> {
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

fn read_edges(path: &Path) -> anyhow::Result<HashMap<i64, Vec<EdgeGeom>>> {
    debug_assert_eq!(GEOM_COLUMNS, "osm_id,seg_idx,start_id,end_id,geom,length_m,total_length_m,cost,reverse_cost");
    let mut reader = csv::Reader::from_path(path)?;
    let mut by_osm_id: HashMap<i64, Vec<EdgeGeom>> = HashMap::new();
    for result in reader.records() {
        let record = result?;
        let geom_hex = &record[4];
        if geom_hex.is_empty() {
            continue;
        }
        let osm_id: i64 = record[0].parse()?;
        let seg_idx: usize = record[1].parse()?;
        let coordinates = lonlat_coordinates(&hex::decode(geom_hex)?)?;
        by_osm_id.entry(osm_id).or_default().push(EdgeGeom { seg_idx, coordinates });
    }
    for segments in by_osm_id.values_mut() {
        segments.sort_by_key(|s| s.seg_idx);
    }
    Ok(by_osm_id)
}

fn merge_properties(target: &mut Map<String, Value>, json_str: &str) {
    if let Ok(Value::Object(map)) = serde_json::from_str(json_str) {
        target.extend(map);
    }
}

/// Reads `{table}.csv` (per `tables`) + `edges.csv` from `out_dir` and writes `{table}.geojson`
/// alongside them: `{"type": "FeatureCollection", "features": [...], "cutPoints": {...}}`.
pub fn write_geojson_from_csv(out_dir: &Path, tables: &[String]) -> anyhow::Result<()> {
    debug_assert_eq!(TAG_COLUMNS, "osm_id,osm_type,id,category,produced,annotations,meta");
    let edges = read_edges(&out_dir.join("edges.csv"))?;
    let nodes_path = out_dir.join("nodes.csv");
    let nodes = if nodes_path.exists() { read_nodes(&nodes_path)? } else { HashMap::new() };
    let members_path = out_dir.join("relation_members.csv");
    let relation_members =
        if members_path.exists() { read_relation_members(&members_path)? } else { HashMap::new() };

    for table in tables {
        let mut reader = csv::Reader::from_path(out_dir.join(format!("{table}.csv")))?;
        let mut features = Vec::new();
        let mut cut_points = Vec::new();

        for result in reader.records() {
            let record = result?;
            let osm_id: i64 = record[0].parse()?;
            let osm_type = &record[1];

            // A relation's own member ways carry its route geometry; a node's own row carries
            // its point. Only ways are looked up directly in `edges` (keyed by way osm_id).
            let segments: Vec<&EdgeGeom> = match osm_type {
                "R" => match relation_members.get(&osm_id) {
                    Some(way_ids) => {
                        way_ids.iter().filter_map(|way_id| edges.get(way_id)).flatten().collect()
                    }
                    None => continue,
                },
                "N" => {
                    let Some(geom) = nodes.get(&osm_id) else { continue };
                    let Ok(coordinates) = lonlat_point(geom) else { continue };

                    let mut properties = Map::new();
                    properties.insert("osm_id".to_owned(), json!(osm_id));
                    properties.insert("id".to_owned(), json!(&record[2]));
                    properties.insert("category".to_owned(), json!(&record[3]));
                    merge_properties(&mut properties, &record[4]);
                    merge_properties(&mut properties, &record[5]);

                    features.push(json!({
                        "type": "Feature",
                        "geometry": { "type": "Point", "coordinates": coordinates },
                        "properties": properties,
                    }));
                    continue;
                }
                _ => match edges.get(&osm_id) {
                    Some(segments) => segments.iter().collect(),
                    None => continue,
                },
            };
            if segments.is_empty() {
                continue;
            }

            let mut properties = Map::new();
            properties.insert("osm_id".to_owned(), json!(osm_id));
            properties.insert("id".to_owned(), json!(&record[2]));
            properties.insert("category".to_owned(), json!(&record[3]));
            merge_properties(&mut properties, &record[4]);
            // `annotations` carries engine-attached bookkeeping (e.g. `_side`, the side-split
            // object's left/right/self side; `<output>_source`/`_confidence` provenance) rather
            // than topic-authored output — merged in alongside `produced` since it's ordinary
            // public output too (e.g. the live editor keys a line-offset expression on `_side`).
            merge_properties(&mut properties, &record[5]);

            for segment in &segments {
                let mut properties = properties.clone();
                properties.insert("seg_idx".to_owned(), json!(segment.seg_idx));
                features.push(json!({
                    "type": "Feature",
                    "geometry": { "type": "LineString", "coordinates": segment.coordinates },
                    "properties": properties,
                }));
            }
            // The point shared by consecutive segments of the same way is where the routing graph
            // cut it — surfaced separately since it's not itself a tagged feature. Only meaningful
            // for a single way's own segments, not a relation's aggregated member-way segments.
            if osm_type != "R" {
                for segment in segments.iter().skip(1) {
                    if let Some(&start) = segment.coordinates.first() {
                        cut_points.push(json!({
                            "type": "Feature",
                            "geometry": { "type": "Point", "coordinates": start },
                            "properties": { "osm_id": osm_id },
                        }));
                    }
                }
            }
        }

        let feature_collection = json!({
            "type": "FeatureCollection",
            "features": features,
            "cutPoints": { "type": "FeatureCollection", "features": cut_points },
        });
        std::fs::write(
            out_dir.join(format!("{table}.geojson")),
            serde_json::to_vec(&feature_collection)?,
        )?;
    }
    Ok(())
}
