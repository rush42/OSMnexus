//! Builds GeoJSON output per topic from the CSV output `--output geojson`/`geojsonseq` shares with
//! `--output csv` (`{table}.csv` tag rows + this topic's own geometry table(s), see `db::schema`'s
//! `way_geom_table`/`point_table`/`polygon_table`/`relation_*_table`) — for local tooling (e.g. the
//! live editor) that wants one self-contained file instead of a table pair. Two sinks share the same
//! `build_features` collection step and differ only in framing: `write_geojson_from_csv` wraps them
//! in one `{table}.geojson` `FeatureCollection` (simple, but buffers the whole topic in memory and
//! isn't streamable); `write_geojsonseq_from_csv` writes one `{table}.geojsonseq` Feature object per
//! line (RFC 8142) instead.
//! Falls back to the shared `edges.csv` graph table for a topic that declared `"way": ["graph"]`
//! instead of `["line"]` — that's the one shape not stored per-topic (see `EDGE_TABLE`'s own doc) —
//! surfacing each way's intersection-split segments plus two kinds of cut-point Feature interleaved
//! into the same stream, each tagged `"kind"` in `properties`: `"cut"` for the node shared between
//! consecutive segments of the same way (where the graph broke it apart), and `"endpoint"` for the
//! way's own two ends (which may or may not coincide with another way — the graph shape alone
//! doesn't say). A topic with per-topic geometry tables has no split points, so no `"kind"`-tagged
//! features are emitted for it.

use std::collections::HashMap;
use std::io::Write;
use std::path::Path;

use serde_json::{json, Map, Value};

use crate::geom::primitives::{linestring_from_ewkb, mercator_to_wgs84, multilinestring_from_ewkb, point_from_ewkb};
use crate::geom::rows::{EDGE_COLUMNS, POINT_COLUMNS, POLYGON_COLUMNS, WAY_COLUMNS};
use crate::output::rows::TAG_COLUMNS;

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

fn lonlat_multi_coordinates(geom: &[u8]) -> anyhow::Result<Vec<Vec<[f64; 2]>>> {
    Ok(multilinestring_from_ewkb(geom)?
        .into_iter()
        .map(|run| {
            run.into_iter()
                .map(|(x, y)| {
                    let (lon, lat) = mercator_to_wgs84(x, y);
                    [lon, lat]
                })
                .collect()
        })
        .collect())
}

fn lonlat_point(geom: &[u8]) -> anyhow::Result<[f64; 2]> {
    let (x, y) = point_from_ewkb(geom)?;
    let (lon, lat) = mercator_to_wgs84(x, y);
    Ok([lon, lat])
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
    debug_assert_eq!(EDGE_COLUMNS, "osm_id,seg_idx,start_id,end_id,geom,length_m,total_length_m,cost,reverse_cost");
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

/// Reads a `{table}_geom.csv`/`{table}_relation_geom.csv` file (`WAY_COLUMNS`: whole-way/relation
/// linestring, no split segments), keyed by `osm_id`. Returns `None` if the file doesn't exist —
/// this topic didn't declare that shape for this kind.
fn read_way_geom(path: &Path) -> anyhow::Result<Option<HashMap<i64, Vec<[f64; 2]>>>> {
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
        by_osm_id.insert(osm_id, lonlat_coordinates(&hex::decode(geom_hex)?)?);
    }
    Ok(Some(by_osm_id))
}

/// Reads a `{table}_relation_geom.csv` file (`WAY_COLUMNS`), keyed by `osm_id` — relation lines are
/// `MultiLineString` (see `db::schema::GeomTableShape::MultiLineString`'s own doc: a relation's
/// member ways chained by shared endpoint frequently assemble into several disconnected runs), so
/// each entry is a list of per-run coordinate lists rather than one flat list like `read_way_geom`.
/// Returns `None` if the file doesn't exist — this topic didn't declare relation line geometry.
fn read_relation_line_geom(path: &Path) -> anyhow::Result<Option<HashMap<i64, Vec<Vec<[f64; 2]>>>>> {
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
        by_osm_id.insert(osm_id, lonlat_multi_coordinates(&hex::decode(geom_hex)?)?);
    }
    Ok(Some(by_osm_id))
}

/// Reads a `{table}_point.csv`/`{table}_relation_point.csv` file (`POINT_COLUMNS`), keyed by
/// `osm_id`. Returns `None` if the file doesn't exist.
fn read_point_geom(path: &Path) -> anyhow::Result<Option<HashMap<i64, [f64; 2]>>> {
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
        by_osm_id.insert(osm_id, lonlat_point(&hex::decode(geom_hex)?)?);
    }
    Ok(Some(by_osm_id))
}

/// Reads a `{table}_polygon.csv`/`{table}_relation_polygon.csv` file (`POLYGON_COLUMNS` — a single
/// ring's worth of coordinates; multipolygon holes aren't reconstructed here), keyed by `osm_id`.
/// Returns `None` if the file doesn't exist.
fn read_polygon_geom(path: &Path) -> anyhow::Result<Option<HashMap<i64, Vec<[f64; 2]>>>> {
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
        by_osm_id.insert(osm_id, lonlat_coordinates(&hex::decode(geom_hex)?)?);
    }
    Ok(Some(by_osm_id))
}

fn merge_properties(target: &mut Map<String, Value>, json_str: &str) {
    if let Ok(Value::Object(map)) = serde_json::from_str(json_str) {
        target.extend(map);
    }
}

fn base_properties(record: &csv::StringRecord, osm_id: i64) -> Map<String, Value> {
    let mut properties = Map::new();
    properties.insert("osm_id".to_owned(), json!(osm_id));
    properties.insert("id".to_owned(), json!(&record[2]));
    // Empty means `accept_all` (no category matched — see `TopicRow::category`); keep the column
    // out of GeoJSON properties entirely rather than emitting a misleading empty string.
    if !record[3].is_empty() {
        properties.insert("category".to_owned(), json!(&record[3]));
    }
    merge_properties(&mut properties, &record[4]);
    // `annotations` carries engine-attached bookkeeping (e.g. `_side`, the side-split object's
    // left/right/self side; `<output>_source`/`_confidence` provenance) rather than topic-authored
    // output — merged in alongside `produced` since it's ordinary public output too (e.g. the live
    // editor keys a line-offset expression on `_side`).
    merge_properties(&mut properties, &record[5]);
    properties
}

/// Reads `{table}.csv` plus whichever of this topic's own geometry tables exist
/// (`{table}_geom`/`_point`/`_polygon` for way/node shapes, `{table}_relation_geom`/
/// `_relation_point`/`_relation_polygon` for relation shapes, or the shared `edges.csv`/`nodes.csv`
/// for a topic that declared `"way": ["graph"]` instead) and collects this topic's GeoJSON
/// `Feature`s, cut points interleaved in with `properties.kind` set to `"cut"`/`"endpoint"`.
/// Shared by both sink framings — see the module doc.
fn build_features(
    out_dir: &Path,
    table: &str,
    edges: &HashMap<i64, Vec<EdgeGeom>>,
    relation_members: &HashMap<i64, Vec<i64>>,
) -> anyhow::Result<Vec<Value>> {
    debug_assert_eq!(TAG_COLUMNS, "osm_id,osm_type,id,category,produced,annotations,meta");
    let way_geom = read_way_geom(&out_dir.join(format!("{table}_geom.csv")))?;
    let way_point = read_point_geom(&out_dir.join(format!("{table}_point.csv")))?;
    let way_polygon = read_polygon_geom(&out_dir.join(format!("{table}_polygon.csv")))?;
    let relation_geom = read_relation_line_geom(&out_dir.join(format!("{table}_relation_geom.csv")))?;
    let relation_point = read_point_geom(&out_dir.join(format!("{table}_relation_point.csv")))?;
    let relation_polygon = read_polygon_geom(&out_dir.join(format!("{table}_relation_polygon.csv")))?;

    let mut reader = csv::Reader::from_path(out_dir.join(format!("{table}.csv")))?;
    let mut features = Vec::new();

    for result in reader.records() {
        let record = result?;
        let osm_id: i64 = record[0].parse()?;
        let osm_type = &record[1];

        match osm_type {
            "R" => {
                if let Some(runs) = relation_geom.as_ref().and_then(|m| m.get(&osm_id)) {
                    features.push(json!({
                        "type": "Feature",
                        "geometry": { "type": "MultiLineString", "coordinates": runs },
                        "properties": base_properties(&record, osm_id),
                    }));
                } else if let Some(coordinates) = relation_polygon.as_ref().and_then(|m| m.get(&osm_id)) {
                    features.push(json!({
                        "type": "Feature",
                        "geometry": { "type": "Polygon", "coordinates": [coordinates] },
                        "properties": base_properties(&record, osm_id),
                    }));
                } else if let Some(coordinates) = relation_point.as_ref().and_then(|m| m.get(&osm_id)) {
                    features.push(json!({
                        "type": "Feature",
                        "geometry": { "type": "Point", "coordinates": coordinates },
                        "properties": base_properties(&record, osm_id),
                    }));
                } else if let Some(way_ids) = relation_members.get(&osm_id) {
                    // Graph-shape fallback: no per-topic relation geometry table, stitch the
                    // shared graph's edges for this relation's member ways instead.
                    let segments: Vec<&EdgeGeom> =
                        way_ids.iter().filter_map(|way_id| edges.get(way_id)).flatten().collect();
                    let properties = base_properties(&record, osm_id);
                    for segment in &segments {
                        let mut properties = properties.clone();
                        properties.insert("seg_idx".to_owned(), json!(segment.seg_idx));
                        features.push(json!({
                            "type": "Feature",
                            "geometry": { "type": "LineString", "coordinates": segment.coordinates },
                            "properties": properties,
                        }));
                    }
                }
            }
            "N" => {
                let Some(coordinates) = way_point.as_ref().and_then(|m| m.get(&osm_id)) else { continue };
                features.push(json!({
                    "type": "Feature",
                    "geometry": { "type": "Point", "coordinates": coordinates },
                    "properties": base_properties(&record, osm_id),
                }));
            }
            _ => {
                if let Some(coordinates) = way_geom.as_ref().and_then(|m| m.get(&osm_id)) {
                    features.push(json!({
                        "type": "Feature",
                        "geometry": { "type": "LineString", "coordinates": coordinates },
                        "properties": base_properties(&record, osm_id),
                    }));
                } else if let Some(coordinates) = way_polygon.as_ref().and_then(|m| m.get(&osm_id)) {
                    features.push(json!({
                        "type": "Feature",
                        "geometry": { "type": "Polygon", "coordinates": [coordinates] },
                        "properties": base_properties(&record, osm_id),
                    }));
                } else if let Some(coordinates) = way_point.as_ref().and_then(|m| m.get(&osm_id)) {
                    features.push(json!({
                        "type": "Feature",
                        "geometry": { "type": "Point", "coordinates": coordinates },
                        "properties": base_properties(&record, osm_id),
                    }));
                } else if let Some(segments) = edges.get(&osm_id) {
                    // Graph-shape fallback (see module doc): split into intersection segments,
                    // surfacing the shared node between consecutive segments as a cut point.
                    let properties = base_properties(&record, osm_id);
                    for segment in segments {
                        let mut properties = properties.clone();
                        properties.insert("seg_idx".to_owned(), json!(segment.seg_idx));
                        features.push(json!({
                            "type": "Feature",
                            "geometry": { "type": "LineString", "coordinates": segment.coordinates },
                            "properties": properties,
                        }));
                    }
                    for segment in segments.iter().skip(1) {
                        if let Some(&start) = segment.coordinates.first() {
                            features.push(json!({
                                "type": "Feature",
                                "geometry": { "type": "Point", "coordinates": start },
                                "properties": { "osm_id": osm_id, "kind": "cut" },
                            }));
                        }
                    }
                    // The way's own two ends — never a "cut" (that's reserved for splits
                    // *within* this way, see above), but still worth surfacing since they may
                    // coincide with another way's endpoint or a cut point of its own.
                    if let Some(first) = segments.first().and_then(|s| s.coordinates.first()) {
                        features.push(json!({
                            "type": "Feature",
                            "geometry": { "type": "Point", "coordinates": first },
                            "properties": { "osm_id": osm_id, "kind": "endpoint" },
                        }));
                    }
                    if let Some(last) = segments.last().and_then(|s| s.coordinates.last()) {
                        features.push(json!({
                            "type": "Feature",
                            "geometry": { "type": "Point", "coordinates": last },
                            "properties": { "osm_id": osm_id, "kind": "endpoint" },
                        }));
                    }
                }
            }
        }
    }

    Ok(features)
}

/// Reads each of `tables`' CSV output (see `build_features`) and writes `{table}.geojsonseq`
/// alongside them: one GeoJSON `Feature` object per line (RFC 8142).
pub fn write_geojsonseq_from_csv(out_dir: &Path, tables: &[String]) -> anyhow::Result<()> {
    let edges_path = out_dir.join("edges.csv");
    let edges = if edges_path.exists() { read_edges(&edges_path)? } else { HashMap::new() };
    let members_path = out_dir.join("relation_members.csv");
    let relation_members =
        if members_path.exists() { read_relation_members(&members_path)? } else { HashMap::new() };

    for table in tables {
        let features = build_features(out_dir, table, &edges, &relation_members)?;
        let mut writer = std::io::BufWriter::new(std::fs::File::create(
            out_dir.join(format!("{table}.geojsonseq")),
        )?);
        for feature in &features {
            serde_json::to_writer(&mut writer, feature)?;
            writer.write_all(b"\n")?;
        }
    }
    Ok(())
}

/// Reads each of `tables`' CSV output (see `build_features`) and writes `{table}.geojson`
/// alongside them: a single `{"type": "FeatureCollection", "features": [...]}` object — simpler to
/// consume whole than `geojsonseq`, at the cost of buffering the whole topic in memory.
pub fn write_geojson_from_csv(out_dir: &Path, tables: &[String]) -> anyhow::Result<()> {
    let edges_path = out_dir.join("edges.csv");
    let edges = if edges_path.exists() { read_edges(&edges_path)? } else { HashMap::new() };
    let members_path = out_dir.join("relation_members.csv");
    let relation_members =
        if members_path.exists() { read_relation_members(&members_path)? } else { HashMap::new() };

    for table in tables {
        let features = build_features(out_dir, table, &edges, &relation_members)?;
        let feature_collection = json!({ "type": "FeatureCollection", "features": features });
        std::fs::write(out_dir.join(format!("{table}.geojson")), serde_json::to_vec(&feature_collection)?)?;
    }
    Ok(())
}
