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

use crate::output::geometry::{linestring_from_ewkb, mercator_to_wgs84};
use crate::output::rows::{GEOM_COLUMNS, TAG_COLUMNS};

struct EdgeGeom {
    seg_idx: usize,
    geom: Vec<u8>,
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
        by_osm_id.entry(osm_id).or_default().push(EdgeGeom { seg_idx, geom: hex::decode(geom_hex)? });
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
    debug_assert_eq!(TAG_COLUMNS, "osm_id,osm_type,id,osm,derived,private,meta");
    let edges = read_edges(&out_dir.join("edges.csv"))?;

    for table in tables {
        let mut reader = csv::Reader::from_path(out_dir.join(format!("{table}.csv")))?;
        let mut features = Vec::new();
        let mut cut_points = Vec::new();

        for result in reader.records() {
            let record = result?;
            let osm_id: i64 = record[0].parse()?;
            let Some(segments) = edges.get(&osm_id) else { continue };

            let mut properties = Map::new();
            properties.insert("osm_id".to_owned(), json!(osm_id));
            properties.insert("id".to_owned(), json!(&record[2]));
            merge_properties(&mut properties, &record[3]);
            merge_properties(&mut properties, &record[4]);

            for segment in segments {
                let Ok(coordinates) = lonlat_coordinates(&segment.geom) else { continue };
                let mut properties = properties.clone();
                properties.insert("seg_idx".to_owned(), json!(segment.seg_idx));
                features.push(json!({
                    "type": "Feature",
                    "geometry": { "type": "LineString", "coordinates": coordinates },
                    "properties": properties,
                }));
            }
            // The point shared by consecutive segments of the same way is where the routing graph
            // cut it — surfaced separately since it's not itself a tagged feature.
            for segment in segments.iter().skip(1) {
                if let Ok(coordinates) = lonlat_coordinates(&segment.geom) {
                    if let Some(&start) = coordinates.first() {
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
