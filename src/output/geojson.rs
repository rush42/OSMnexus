//! Builds GeoJSON output per topic from the staged output `--output geojson`/`geojsonseq` write
//! during the main run (`{table}.bin` tag rows + this topic's own `{table}_node_geom`/
//! `{table}_way_geom`/`{table}_relation_geom` tables, plus the shared `edges.bin`/
//! `relation_members.bin` — the pipeline's own staging format, see `output::stage`) — for local
//! tooling (e.g. the live editor) that wants one self-contained file instead of a table pair. Both
//! sinks share the same `for_each_feature` pass and differ only in framing: `write_geojson` wraps
//! the features in one `{table}.geojson` `FeatureCollection`, `write_geojsonseq` writes one
//! `{table}.geojsonseq` Feature object per line (RFC 8142). Neither buffers the topic — see
//! `for_each_feature`'s own doc.
//!
//! The tag/geometry files are read forward, in one pass each, never randomly seeked — safe because
//! `geom::materialize` resolves+routes a table's node/way/relation geometry (and the shared graph's
//! edges) in the exact same order the select phase already routed that element's tag row in
//! (`SelectionContext::kept_way_order`/`kept_relation_order`), so each geometry file's `osm_id`
//! sequence is a *subset* of `{table}.bin`'s same-kind row sequence, in matching relative order. No
//! full in-memory geometry map needed to match them up — `output::cursor::OrderedGeomCursor` (node/
//! way/relation geometry) and `EdgeCursor` (a way's own graph-fallback segments) both walk forward in
//! lockstep with the tag-row stream instead; `output::parquet` reuses the exact same cursors for its
//! own `.parquet` output. `{table}.bin` itself is read streaming off disk (a `BufReader`, not loaded
//! whole) since it can be gigabytes for a country-sized run; `edges.bin`/`relation_members.bin` are
//! read fully into memory once — both far smaller (see this session's own benchmarking notes), and a
//! relation's graph-fallback geometry needs random access by arbitrary member way id, which no
//! forward cursor can serve (a relation's member ways aren't a subsequence of any single ordered
//! pass).

use std::collections::HashMap;
use std::io::Write;
use std::path::Path;

use serde_json::{json, Map, Value};

use crate::output::cursor::{
    group_edges_by_way, read_edges, read_relation_members, EdgeCursor, EdgeGeom, GeomValue,
    OrderedGeomCursor,
};
use crate::output::rows::TopicRow;
use crate::output::stage::StageReader;

fn merge_properties(target: &mut Map<String, Value>, json_str: &str) {
    if !json_str.is_empty() {
        if let Ok(Value::Object(map)) = serde_json::from_str(json_str) {
            target.extend(map);
        }
    }
}

fn base_properties(row: &TopicRow) -> Map<String, Value> {
    let mut properties = Map::new();
    properties.insert("osm_id".to_owned(), json!(row.osm_id));
    if let Some(id) = &row.id {
        properties.insert("id".to_owned(), json!(id));
    }
    // `None` means `accept_all` (no category matched — see `TopicRow::category`); keep the column
    // out of GeoJSON properties entirely rather than emitting a misleading empty string.
    if let Some(category) = &row.category {
        properties.insert("category".to_owned(), json!(category.as_ref()));
    }
    merge_properties(&mut properties, &row.produced);
    // `annotations` carries engine-attached bookkeeping (e.g. `_side`, the side-split object's
    // left/right/self side; `<output>_source`/`_confidence` provenance) rather than topic-authored
    // output — merged in alongside `produced` since it's ordinary public output too (e.g. the live
    // editor keys a line-offset expression on `_side`).
    merge_properties(&mut properties, &row.annotations);
    properties
}

/// Push the graph-shape fallback's Features (see module doc) for `segments` — a way's own segments,
/// or (for a relation) the pooled segments of every member way that has one.
fn emit_graph_fallback_features<F>(
    emit: &mut F,
    segments: &[&EdgeGeom],
    properties: &Map<String, Value>,
) -> anyhow::Result<()>
where
    F: FnMut(&Value) -> anyhow::Result<()>,
{
    for segment in segments {
        let mut properties = properties.clone();
        properties.insert("seg_idx".to_owned(), json!(segment.seg_idx));
        emit(&json!({
            "type": "Feature",
            "geometry": { "type": "LineString", "coordinates": segment.coordinates },
            "properties": properties,
        }))?;
    }
    Ok(())
}

/// Reads `{table}.bin` plus whichever of this topic's own geometry tables exist
/// (`{table}_node_geom`/`{table}_way_geom`/`{table}_relation_geom`, or the shared `edges.bin` for a
/// topic that declared `"graph": { "way": true }` without its own way geometry table) and collects
/// this topic's GeoJSON `Feature`s, cut points interleaved in with `properties.kind` set to
/// `"cut"`/`"endpoint"`. Shared by both sink framings — see the module doc.
///
/// Hands each feature to `emit` as it is produced rather than returning a `Vec<Value>`. The
/// collecting version held every feature of a topic live at once: on `germany-latest.osm.pbf` that
/// measured **43.7GB** of heap for `geojsonseq` and **92.0GB** for `geojson` (`RssAnon`, not page
/// cache — the peak lands in this phase, after the PBF mmap is gone). `serde_json::Value` is a very
/// fat representation of a coordinate list, so a topic's features cost far more resident than the
/// `.geojsonseq` bytes they serialize to. Streaming makes both framings bounded by one feature.
fn for_each_feature<F>(
    out_dir: &Path,
    table: &str,
    edges: &[(i64, EdgeGeom)],
    edges_by_way: &HashMap<i64, Vec<&EdgeGeom>>,
    relation_members: &HashMap<i64, Vec<i64>>,
    mut emit: F,
) -> anyhow::Result<()>
where
    F: FnMut(&Value) -> anyhow::Result<()>,
{
    let mut node_geom = OrderedGeomCursor::open(&out_dir.join(format!("{table}_node_geom.bin")))?;
    let mut way_geom = OrderedGeomCursor::open(&out_dir.join(format!("{table}_way_geom.bin")))?;
    let mut relation_geom = OrderedGeomCursor::open(&out_dir.join(format!("{table}_relation_geom.bin")))?;
    let mut edge_cursor = EdgeCursor::new(edges);

    let mut tags = StageReader::<TopicRow>::open(&out_dir.join(format!("{table}.bin")))?;

    while let Some(row) = tags.next_row()? {
        let osm_id = row.osm_id;

        match row.osm_type {
            "R" => {
                let geom = match relation_geom.as_mut() {
                    Some(cursor) => cursor.get(osm_id)?,
                    None => None,
                };
                if let Some(geom) = geom {
                    emit(&json!({
                        "type": "Feature",
                        "geometry": geom.to_geojson(),
                        "properties": base_properties(&row),
                    }))?;
                } else if let Some(way_ids) = relation_members.get(&osm_id) {
                    // Graph-shape fallback: no per-topic relation geometry table, stitch the
                    // shared graph's edges for this relation's member ways instead.
                    let segments: Vec<&EdgeGeom> =
                        way_ids.iter().filter_map(|way_id| edges_by_way.get(way_id)).flatten().copied().collect();
                    emit_graph_fallback_features(&mut emit, &segments, &base_properties(&row))?;
                }
            }
            "N" => {
                let geom = match node_geom.as_mut() {
                    Some(cursor) => cursor.get(osm_id)?,
                    None => None,
                };
                let Some(GeomValue::Point(p)) = geom else { continue };
                emit(&json!({
                    "type": "Feature",
                    "geometry": { "type": "Point", "coordinates": p },
                    "properties": base_properties(&row),
                }))?;
            }
            _ => {
                let geom = match way_geom.as_mut() {
                    Some(cursor) => cursor.get(osm_id)?,
                    None => None,
                };
                if let Some(geom) = geom {
                    emit(&json!({
                        "type": "Feature",
                        "geometry": geom.to_geojson(),
                        "properties": base_properties(&row),
                    }))?;
                } else {
                    // Graph-shape fallback (see module doc): split into intersection segments,
                    // surfacing the shared node between consecutive segments as a cut point.
                    let segments = edge_cursor.get_all(osm_id);
                    if !segments.is_empty() {
                        let properties = base_properties(&row);
                        emit_graph_fallback_features(&mut emit, &segments, &properties)?;
                        for segment in segments.iter().skip(1) {
                            if let Some(&start) = segment.coordinates.first() {
                                emit(&json!({
                                    "type": "Feature",
                                    "geometry": { "type": "Point", "coordinates": start },
                                    "properties": { "osm_id": osm_id, "kind": "cut" },
                                }))?;
                            }
                        }
                        // The way's own two ends — never a "cut" (that's reserved for splits
                        // *within* this way, see above), but still worth surfacing since they may
                        // coincide with another way's endpoint or a cut point of its own.
                        if let Some(first) = segments.first().and_then(|s| s.coordinates.first()) {
                            emit(&json!({
                                "type": "Feature",
                                "geometry": { "type": "Point", "coordinates": first },
                                "properties": { "osm_id": osm_id, "kind": "endpoint" },
                            }))?;
                        }
                        if let Some(last) = segments.last().and_then(|s| s.coordinates.last()) {
                            emit(&json!({
                                "type": "Feature",
                                "geometry": { "type": "Point", "coordinates": last },
                                "properties": { "osm_id": osm_id, "kind": "endpoint" },
                            }))?;
                        }
                    }
                }
            }
        }
    }

    Ok(())
}

/// Reads each of `tables`' staged output (see `for_each_feature`) and writes `{table}.geojsonseq`
/// alongside them: one GeoJSON `Feature` object per line (RFC 8142).
pub fn write_geojsonseq(out_dir: &Path, tables: &[String]) -> anyhow::Result<()> {
    let edges = read_edges(&out_dir.join("edges.bin"))?;
    let edges_by_way = group_edges_by_way(&edges);
    let relation_members = read_relation_members(&out_dir.join("relation_members.bin"))?;

    for table in tables {
        let mut writer = std::io::BufWriter::new(std::fs::File::create(out_dir.join(format!("{table}.geojsonseq")))?);
        for_each_feature(out_dir, table, &edges, &edges_by_way, &relation_members, |feature| {
            serde_json::to_writer(&mut writer, feature)?;
            writer.write_all(b"\n")?;
            Ok(())
        })?;
        writer.flush()?;
    }
    Ok(())
}

/// Reads each of `tables`' staged output (see `for_each_feature`) and writes `{table}.geojson`
/// alongside them: a single `{"type": "FeatureCollection", "features": [...]}` object — simpler to
/// consume whole than `geojsonseq`, and streamed into just the same (see the framing note below).
pub fn write_geojson(out_dir: &Path, tables: &[String]) -> anyhow::Result<()> {
    let edges = read_edges(&out_dir.join("edges.bin"))?;
    let edges_by_way = group_edges_by_way(&edges);
    let relation_members = read_relation_members(&out_dir.join("relation_members.bin"))?;

    for table in tables {
        let mut writer = std::io::BufWriter::new(std::fs::File::create(out_dir.join(format!("{table}.geojson")))?);
        // Framing written by hand so the features can stream into the array rather than being
        // collected and handed to `serde_json::to_vec` as one `Value`. Key order matches what
        // `json!({"type": .., "features": ..})` produced: `serde_json::Map` is a `BTreeMap` here
        // (the `preserve_order` feature is off — no `indexmap` in its dependency list), so it
        // serialized its keys sorted, i.e. `features` before `type`.
        writer.write_all(br#"{"features":["#)?;
        let mut first = true;
        for_each_feature(out_dir, table, &edges, &edges_by_way, &relation_members, |feature| {
            if !first {
                writer.write_all(b",")?;
            }
            first = false;
            serde_json::to_writer(&mut writer, feature)?;
            Ok(())
        })?;
        writer.write_all(br#"],"type":"FeatureCollection"}"#)?;
        writer.flush()?;
    }
    Ok(())
}
