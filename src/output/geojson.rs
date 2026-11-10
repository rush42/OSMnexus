//! Builds GeoJSON output per topic from the staged output `--output geojson`/`geojsonseq` write
//! during the main run (`{table}.bin` tag rows + this topic's own `{table}_node_geom`/
//! `{table}_way_geom`/`{table}_relation_geom` tables, plus the shared `edges.bin` — the pipeline's
//! own staging format, see `output::stage`) — for local
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
//! whole) since it can be gigabytes for a country-sized run; `edges.bin` is read into memory once.
//!
//! A relation is written with its own geometry (`"geometry_output": { "relation": ... }`) or not at
//! all. It used to fall back to borrowing its member ways' graph segments, which needed random
//! access by member way id; `inherit_to_member` replaced that by putting the relation's context on
//! the member ways' own rows (see `topic::inherit`), where every geometry join is the element's
//! own.

use std::io::Write;
use std::path::Path;

use serde_json::value::RawValue;

use crate::config::CoordPrecision;
use crate::geom::coord::{Coord, Geom};
use crate::output::coord_fmt::CoordFmt;
use crate::output::cursor::{
    read_edges, EdgeCursor, EdgeGeom, OrderedGeomCursor,
};
use crate::output::rows::TopicRow;
use crate::output::stage::StageReader;

/// One feature's geometry, as one of the three shapes this module emits: a joined geometry row, a
/// single graph-edge segment, or a bare point (a cut/endpoint marker).
enum FeatureGeometry<'a> {
    Geom(&'a Geom),
    Segment(&'a [Coord]),
    Point(Coord),
}

pub struct Feature<'a> {
    geometry: FeatureGeometry<'a>,
    properties: &'a Properties<'a>,
}

impl Feature<'_> {
    /// `{"geometry":…,"properties":…,"type":"Feature"}` into `buf` — keys sorted and compact,
    /// byte-for-byte what serializing it through `serde_json` produced before geometry was written
    /// by hand. The geometry is written straight from integer coordinates (`output::coord_fmt`);
    /// only `properties` still goes through `serde_json`.
    fn write(&self, buf: &mut Vec<u8>, fmt: &CoordFmt) -> anyhow::Result<()> {
        buf.extend_from_slice(br#"{"geometry":"#);
        match &self.geometry {
            FeatureGeometry::Geom(g) => fmt.write_json_geometry(buf, g),
            FeatureGeometry::Segment(coords) => {
                buf.extend_from_slice(br#"{"coordinates":"#);
                fmt.write_json_run(buf, coords);
                buf.extend_from_slice(br#","type":"LineString"}"#);
            }
            FeatureGeometry::Point(c) => {
                buf.extend_from_slice(br#"{"coordinates":"#);
                fmt.write_json_coord(buf, *c);
                buf.extend_from_slice(br#","type":"Point"}"#);
            }
        }
        buf.extend_from_slice(br#","properties":"#);
        serde_json::to_writer(&mut *buf, self.properties)?;
        buf.extend_from_slice(br#","type":"Feature"}"#);
        Ok(())
    }
}

fn marker_properties(osm_id: i64, kind: &'static str) -> Properties<'static> {
    let mut properties = Properties::new();
    properties.insert("kind".to_owned(), PropValue::Str(kind));
    properties.insert("osm_id".to_owned(), PropValue::Int(osm_id));
    properties
}

/// One entry in a feature's `properties`. `Raw` is the point of this type: `TopicRow::produced`/
/// `annotations` are *already* JSON text (pre-serialized on the classify workers — see the field's
/// own doc), so their values are spliced through as borrowed slices of that text rather than parsed
/// into a `serde_json::Value` and printed straight back out. Only the keys are parsed, because the
/// keys are what carries the sort order and the later-wins dedup this map relies on.
#[derive(Clone)]
enum PropValue<'a> {
    Raw(&'a RawValue),
    Int(i64),
    Str(&'a str),
}

impl serde::Serialize for PropValue<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            PropValue::Raw(raw) => raw.serialize(serializer),
            PropValue::Int(v) => serializer.serialize_i64(*v),
            PropValue::Str(v) => serializer.serialize_str(v),
        }
    }
}

/// A feature's `properties` object. A `BTreeMap`, matching what `serde_json::Map` was here (its
/// `preserve_order` feature is off), so keys still serialize sorted and a later insert still wins.
type Properties<'a> = std::collections::BTreeMap<String, PropValue<'a>>;

/// Merge one pre-serialized JSON object's entries into `target`. Parse failures and non-object
/// values are ignored rather than reported — unchanged behaviour, and load-bearing: a topic is free
/// to leave these empty, and a malformed value should drop the property, not fail the run.
fn merge_properties<'a>(target: &mut Properties<'a>, json_str: &'a str) {
    if json_str.is_empty() {
        return;
    }
    if let Ok(map) = serde_json::from_str::<std::collections::BTreeMap<String, &'a RawValue>>(json_str) {
        target.extend(map.into_iter().map(|(k, v)| (k, PropValue::Raw(v))));
    }
}

fn base_properties(row: &TopicRow) -> Properties<'_> {
    let mut properties = Properties::new();
    properties.insert("osm_id".to_owned(), PropValue::Int(row.osm_id));
    if let Some(id) = &row.id {
        properties.insert("id".to_owned(), PropValue::Str(id));
    }
    // `None` means `accept_all` (no category matched — see `TopicRow::category`); keep the column
    // out of GeoJSON properties entirely rather than emitting a misleading empty string.
    if let Some(category) = &row.category {
        properties.insert("category".to_owned(), PropValue::Str(category.as_ref()));
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
    properties: &Properties<'_>,
) -> anyhow::Result<()>
where
    F: FnMut(&Feature<'_>) -> anyhow::Result<()>,
{
    if segments.is_empty() {
        return Ok(());
    }
    // Cloned once for the whole run of segments, not once per segment: the only thing that differs
    // between them is `seg_idx`, so seed the key here and overwrite its value in place below. A
    // way's segment count is unbounded, and the clone copies an owned `String` per property key.
    let mut properties = properties.clone();
    properties.insert("seg_idx".to_owned(), PropValue::Int(0));
    for segment in segments {
        // Present since the insert above, so this is an in-place write with no allocation. Every
        // other entry is left exactly as the caller built it.
        if let Some(seg_idx) = properties.get_mut("seg_idx") {
            *seg_idx = PropValue::Int(segment.seg_idx as i64);
        }
        emit(&Feature {
            geometry: FeatureGeometry::Segment(&segment.coordinates),
            properties: &properties,
        })?;
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
///
/// `emit` takes a borrowed [`Feature`], not a `Value`: features are written straight from the
/// cursors' native types — geometry from the staged integer coordinates (`Feature::write`) — so no
/// `Value` tree is built even transiently; such a tree turned every coordinate pair into its own
/// heap-allocated `Vec<Value>`, ~10^8 short-lived allocations on a country-sized run.
fn for_each_feature<F>(
    out_dir: &Path,
    table: &str,
    edges: &[(i64, EdgeGeom)],
    mut emit: F,
) -> anyhow::Result<()>
where
    F: FnMut(&Feature<'_>) -> anyhow::Result<()>,
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
                // No geometry of its own → no feature, same as a node without a point.
                if let Some(geom) = geom {
                    let properties = base_properties(&row);
                    emit(&Feature { geometry: FeatureGeometry::Geom(geom), properties: &properties })?;
                }
            }
            "N" => {
                let geom = match node_geom.as_mut() {
                    Some(cursor) => cursor.get(osm_id)?,
                    None => None,
                };
                // Still filtered to `Point`: a node's geometry table can only hold points, and a
                // non-point row here would be a bug rather than something to render.
                let Some(geom @ Geom::Point(_)) = geom else { continue };
                let properties = base_properties(&row);
                emit(&Feature { geometry: FeatureGeometry::Geom(geom), properties: &properties })?;
            }
            _ => {
                let geom = match way_geom.as_mut() {
                    Some(cursor) => cursor.get(osm_id)?,
                    None => None,
                };
                if let Some(geom) = geom {
                    let properties = base_properties(&row);
                    emit(&Feature { geometry: FeatureGeometry::Geom(geom), properties: &properties })?;
                } else {
                    // Graph-shape fallback (see module doc): split into intersection segments,
                    // surfacing the shared node between consecutive segments as a cut point.
                    let segments = edge_cursor.get_all(osm_id);
                    if !segments.is_empty() {
                        let properties = base_properties(&row);
                        emit_graph_fallback_features(&mut emit, &segments, &properties)?;
                        for segment in segments.iter().skip(1) {
                            if let Some(&start) = segment.coordinates.first() {
                                let properties = marker_properties(osm_id, "cut");
                                emit(&Feature {
                                    geometry: FeatureGeometry::Point(start),
                                    properties: &properties,
                                })?;
                            }
                        }
                        // The way's own two ends — never a "cut" (that's reserved for splits
                        // *within* this way, see above), but still worth surfacing since they may
                        // coincide with another way's endpoint or a cut point of its own.
                        if let Some(&first) = segments.first().and_then(|s| s.coordinates.first()) {
                            let properties = marker_properties(osm_id, "endpoint");
                            emit(&Feature {
                                geometry: FeatureGeometry::Point(first),
                                properties: &properties,
                            })?;
                        }
                        if let Some(&last) = segments.last().and_then(|s| s.coordinates.last()) {
                            let properties = marker_properties(osm_id, "endpoint");
                            emit(&Feature {
                                geometry: FeatureGeometry::Point(last),
                                properties: &properties,
                            })?;
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
pub fn write_geojsonseq(out_dir: &Path, tables: &[String], precision: CoordPrecision) -> anyhow::Result<()> {
    let edges = read_edges(&out_dir.join("edges.bin"))?;
    let fmt = CoordFmt::new(precision);
    let mut buf = Vec::with_capacity(4096);

    for table in tables {
        let mut writer = std::io::BufWriter::new(std::fs::File::create(out_dir.join(format!("{table}.geojsonseq")))?);
        for_each_feature(out_dir, table, &edges, |feature| {
            buf.clear();
            feature.write(&mut buf, &fmt)?;
            buf.push(b'\n');
            writer.write_all(&buf)?;
            Ok(())
        })?;
        writer.flush()?;
    }
    Ok(())
}

/// Reads each of `tables`' staged output (see `for_each_feature`) and writes `{table}.geojson`
/// alongside them: a single `{"type": "FeatureCollection", "features": [...]}` object — simpler to
/// consume whole than `geojsonseq`, and streamed into just the same (see the framing note below).
pub fn write_geojson(out_dir: &Path, tables: &[String], precision: CoordPrecision) -> anyhow::Result<()> {
    let edges = read_edges(&out_dir.join("edges.bin"))?;
    let fmt = CoordFmt::new(precision);
    let mut buf = Vec::with_capacity(4096);

    for table in tables {
        let mut writer = std::io::BufWriter::new(std::fs::File::create(out_dir.join(format!("{table}.geojson")))?);
        // Framing written by hand so the features can stream into the array rather than being
        // collected and handed to `serde_json::to_vec` as one `Value`. Key order matches what
        // `json!({"type": .., "features": ..})` produced: `serde_json::Map` is a `BTreeMap` here
        // (the `preserve_order` feature is off — no `indexmap` in its dependency list), so it
        // serialized its keys sorted, i.e. `features` before `type`.
        writer.write_all(br#"{"features":["#)?;
        let mut first = true;
        for_each_feature(out_dir, table, &edges, |feature| {
            buf.clear();
            if !first {
                buf.push(b',');
            }
            first = false;
            feature.write(&mut buf, &fmt)?;
            writer.write_all(&buf)?;
            Ok(())
        })?;
        writer.write_all(br#"],"type":"FeatureCollection"}"#)?;
        writer.flush()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // No shipped config declares `"graph"`, so nothing in `configs/` reaches the graph-fallback
    // path and the bremen byte-identity gate every other output change is verified against says
    // nothing about it. These tests are that path's only coverage.

    /// `x` in whole degrees.
    fn seg(seg_idx: usize, x: i32) -> EdgeGeom {
        let d = 10_000_000;
        EdgeGeom { seg_idx, coordinates: vec![[x * d, 0], [(x + 1) * d, d]] }
    }

    fn feature_string(feature: &Feature<'_>) -> String {
        let mut buf = Vec::new();
        feature.write(&mut buf, &CoordFmt::new(CoordPrecision(21))).unwrap();
        String::from_utf8(buf).unwrap()
    }

    fn base_props() -> Properties<'static> {
        let mut properties = Properties::new();
        properties.insert("category".to_owned(), PropValue::Str("road"));
        properties.insert("osm_id".to_owned(), PropValue::Int(7));
        properties
    }

    fn emitted(segments: &[&EdgeGeom], properties: &Properties<'_>) -> Vec<String> {
        let mut out = Vec::new();
        emit_graph_fallback_features(
            &mut |feature: &Feature<'_>| {
                out.push(feature_string(feature));
                Ok(())
            },
            segments,
            properties,
        )
        .unwrap();
        out
    }

    #[test]
    fn each_segment_gets_its_own_seg_idx_and_its_own_coordinates() {
        let (a, b, c) = (seg(0, 0), seg(1, 10), seg(2, 20));
        let out = emitted(&[&a, &b, &c], &base_props());
        assert_eq!(out.len(), 3);
        for (i, feature) in out.iter().enumerate() {
            assert!(feature.contains(&format!(r#""seg_idx":{i}"#)), "segment {i}: {feature}");
            assert!(feature.contains(r#""type":"LineString""#));
            assert!(feature.contains(r#""category":"road""#), "base properties lost: {feature}");
            assert!(feature.contains(r#""osm_id":7"#));
        }
        assert!(out[1].contains("[10.0,0.0]"), "wrong coordinates: {}", out[1]);
    }

    /// The reason the shared-buffer rewrite needs a test at all: reusing one map across segments
    /// must not let one segment's `seg_idx` survive into the next feature.
    #[test]
    fn seg_idx_does_not_leak_between_segments() {
        let (a, b) = (seg(0, 0), seg(5, 1));
        let out = emitted(&[&a, &b], &base_props());
        assert_eq!(out[0].matches(r#""seg_idx""#).count(), 1);
        assert!(out[0].contains(r#""seg_idx":0"#));
        assert!(!out[0].contains(r#""seg_idx":5"#));
        assert!(out[1].contains(r#""seg_idx":5"#));
        assert!(!out[1].contains(r#""seg_idx":0"#));
    }

    #[test]
    fn a_seg_idx_already_in_the_base_properties_is_overwritten_not_duplicated() {
        let mut properties = base_props();
        properties.insert("seg_idx".to_owned(), PropValue::Int(99));
        let a = seg(3, 0);
        let out = emitted(&[&a], &properties);
        assert_eq!(out[0].matches(r#""seg_idx""#).count(), 1);
        assert!(out[0].contains(r#""seg_idx":3"#));
        assert!(!out[0].contains("99"));
    }

    #[test]
    fn no_segments_emits_no_features() {
        assert!(emitted(&[], &base_props()).is_empty());
    }

    /// Keys serialize sorted (`Properties` is a `BTreeMap`), so `kind` precedes `osm_id`. Worth
    /// pinning: everywhere else this ordering is held in place by the byte-identity gate, which
    /// does not reach these features.
    #[test]
    fn marker_properties_serialize_kind_before_osm_id() {
        let properties = marker_properties(42, "cut");
        let json = feature_string(&Feature {
            geometry: FeatureGeometry::Point([10_000_000, 20_000_000]),
            properties: &properties,
        });
        assert_eq!(
            json,
            r#"{"geometry":{"coordinates":[1.0,2.0],"type":"Point"},"properties":{"kind":"cut","osm_id":42},"type":"Feature"}"#
        );
    }
}
