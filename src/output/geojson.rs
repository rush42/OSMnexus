//! Builds GeoJSON output per topic from the binary-staged output `--output geojson`/`geojsonseq`
//! write during the main run (`{table}.bin` tag rows + this topic's own `{table}_node_geom`/
//! `{table}_way_geom`/`{table}_relation_geom` tables, plus the shared `edges.bin`/
//! `relation_members.bin` — same wire format `--output pg` streams to Postgres, just written to a
//! file instead of a live connection, see `output::rows`' own doc) — for local tooling (e.g. the
//! live editor) that wants one self-contained file instead of a table pair. Two sinks share the same
//! `build_features` collection step and differ only in framing: `write_geojson` wraps them in one
//! `{table}.geojson` `FeatureCollection` (simple, but buffers the whole topic in memory and isn't
//! streamable); `write_geojsonseq` writes one `{table}.geojsonseq` Feature object per line
//! (RFC 8142) instead.
//!
//! The tag/geometry files are read forward, in one pass each, never randomly seeked — safe because
//! `geom::materialize` resolves+routes a table's node/way/relation geometry (and the shared graph's
//! edges) in the exact same order the select phase already routed that element's tag row in
//! (`SelectionContext::kept_way_order`/`kept_relation_order`), so each geometry file's `osm_id`
//! sequence is a *subset* of `{table}.bin`'s same-kind row sequence, in matching relative order. No
//! full in-memory geometry map needed to match them up — `OrderedGeomCursor` (node/way/relation
//! geometry) and `EdgeCursor` (a way's own graph-fallback segments) both walk forward in lockstep
//! with the tag-row stream instead. `{table}.bin` itself is read streaming off disk (a `BufReader`,
//! not loaded whole) since it can be gigabytes for a country-sized run; `edges.bin`/
//! `relation_members.bin` are read fully into memory once — both far smaller (see this session's own
//! benchmarking notes), and a relation's graph-fallback geometry needs random access by arbitrary
//! member way id, which no forward cursor can serve (a relation's member ways aren't a subsequence
//! of any single ordered pass).

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;

use serde_json::{json, Map, Value};

use crate::geom::primitives::{linestring_from_ewkb, mercator_to_wgs84, multilinestring_from_ewkb, point_from_ewkb};
use crate::geom::rows::{edge_binary_schema, geom_binary_schema, EdgeRow, GeomRow};
use crate::output::rows::{
    member_binary_schema, read_binary_header, read_binary_row, tag_binary_schema, BinaryFieldType,
    FromBinaryRow, MemberRow, TopicRow,
};

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

/// Reads `relation_members.bin` (`relation_osm_id,way_osm_id`), keyed by `relation_osm_id`. `None`
/// (empty map) if the file doesn't exist — no topic declared relation categories at all.
fn read_relation_members(path: &Path) -> anyhow::Result<HashMap<i64, Vec<i64>>> {
    if !path.exists() {
        return Ok(HashMap::new());
    }
    let mut reader = BufReader::new(File::open(path)?);
    read_binary_header(&mut reader)?;
    let schema = member_binary_schema();
    let mut by_relation_id: HashMap<i64, Vec<i64>> = HashMap::new();
    while let Some(fields) = read_binary_row(&mut reader, &schema)? {
        let row = MemberRow::from_binary_fields(fields)?;
        by_relation_id.entry(row.relation_osm_id).or_default().push(row.way_osm_id);
    }
    Ok(by_relation_id)
}

/// Reads `edges.bin` into one flat sequence, in file order — which is `kept_way_order` order (see
/// this module's own doc), i.e. every way's own segments are contiguous and `seg_idx`-ascending
/// already, with no `sort_by_key` needed to restore that (unlike the old CSV path, which couldn't
/// trust file order and re-sorted). `None`/empty if the file doesn't exist — no topic declared
/// `"graph": { "way": true }`.
fn read_edges(path: &Path) -> anyhow::Result<Vec<(i64, EdgeGeom)>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let mut reader = BufReader::new(File::open(path)?);
    read_binary_header(&mut reader)?;
    let schema = edge_binary_schema();
    let mut out = Vec::new();
    while let Some(fields) = read_binary_row(&mut reader, &schema)? {
        let row = EdgeRow::from_binary_fields(fields)?;
        if row.geom_ewkb.is_empty() {
            continue; // a way whose shape degenerated to nothing (see `WayGeometry`'s own doc)
        }
        let coordinates = lonlat_coordinates(&row.geom_ewkb)?;
        out.push((row.osm_id, EdgeGeom { seg_idx: row.seg_idx, coordinates }));
    }
    Ok(out)
}

/// Groups `edges`' flat sequence by way id — for a relation's graph-fallback geometry, which needs
/// random access by arbitrary (non-adjacent) member way id and so can't use `EdgeCursor`'s forward
/// walk (see this module's own doc).
fn group_edges_by_way(edges: &[(i64, EdgeGeom)]) -> HashMap<i64, Vec<&EdgeGeom>> {
    let mut map: HashMap<i64, Vec<&EdgeGeom>> = HashMap::new();
    for (id, g) in edges {
        map.entry(*id).or_default().push(g);
    }
    map
}

/// Forward cursor over `edges`' flat sequence, consumed in step with `{table}.bin`'s own way rows —
/// the graph-fallback equivalent of `OrderedGeomCursor` for the *own-way* case (a way asking for its
/// own segments; see this module's own doc for why the relation-member case still needs
/// `group_edges_by_way`'s hashmap instead). `get_all`, not `get`: unlike node/way/relation geometry
/// (one row, or a repeat of the same one), a way can legitimately own several contiguous segments.
struct EdgeCursor<'a> {
    edges: &'a [(i64, EdgeGeom)],
    pos: usize,
    /// Same repeat-reuse reasoning as `OrderedGeomCursor::last` — a side-split topic asks for the
    /// same way's segments once per side.
    last: Option<(i64, Vec<&'a EdgeGeom>)>,
}

impl<'a> EdgeCursor<'a> {
    fn new(edges: &'a [(i64, EdgeGeom)]) -> Self {
        EdgeCursor { edges, pos: 0, last: None }
    }

    /// This way's segments, in order — empty if `edges` has none for it. Must be called with
    /// `osm_id` in the same relative order `{table}.bin`'s own-kind rows come in.
    fn get_all(&mut self, osm_id: i64) -> Vec<&'a EdgeGeom> {
        if let Some((id, segs)) = &self.last {
            if *id == osm_id {
                return segs.clone();
            }
        }
        if self.pos >= self.edges.len() || self.edges[self.pos].0 != osm_id {
            return Vec::new();
        }
        let start = self.pos;
        while self.pos < self.edges.len() && self.edges[self.pos].0 == osm_id {
            self.pos += 1;
        }
        let segs: Vec<&'a EdgeGeom> = self.edges[start..self.pos].iter().map(|(_, g)| g).collect();
        self.last = Some((osm_id, segs.clone()));
        segs
    }
}

/// One decoded row of a `{table}_node_geom`/`{table}_way_geom`/`{table}_relation_geom` file —
/// `geom_type` says which GeoJSON `"type"` `coordinates` decodes to (`Point` → `[f64; 2]`,
/// `LineString`/`Polygon` → `Vec<[f64; 2]>`, `MultiLineString` → `Vec<Vec<[f64; 2]>>`, hence the enum
/// rather than one fixed shape).
enum GeomValue {
    Point([f64; 2]),
    Line(Vec<[f64; 2]>),
    MultiLine(Vec<Vec<[f64; 2]>>),
    Polygon(Vec<[f64; 2]>),
}

impl GeomValue {
    fn decode(geom_type: &str, geom_ewkb: &[u8]) -> anyhow::Result<Self> {
        Ok(match geom_type {
            "Point" => GeomValue::Point(lonlat_point(geom_ewkb)?),
            "LineString" => GeomValue::Line(lonlat_coordinates(geom_ewkb)?),
            "MultiLineString" => GeomValue::MultiLine(lonlat_multi_coordinates(geom_ewkb)?),
            "Polygon" => GeomValue::Polygon(lonlat_coordinates(geom_ewkb)?),
            other => anyhow::bail!("unknown geom_type {other:?} in geometry table"),
        })
    }

    fn to_geojson(&self) -> Value {
        match self {
            GeomValue::Point(p) => json!({ "type": "Point", "coordinates": p }),
            GeomValue::Line(l) => json!({ "type": "LineString", "coordinates": l }),
            GeomValue::MultiLine(m) => json!({ "type": "MultiLineString", "coordinates": m }),
            GeomValue::Polygon(p) => json!({ "type": "Polygon", "coordinates": [p] }),
        }
    }
}

/// A `{table}_node_geom.bin`/`{table}_way_geom.bin`/`{table}_relation_geom.bin` reader consumed
/// forward, in step with `{table}.bin`'s own rows of that element kind — see this module's own doc
/// for why that's safe. One geometry table per kind (not per (kind, shape) pair — see `GeomRow`'s
/// own doc), so this single cursor type covers node/way/relation geometry alike; the shape a given
/// row decodes to comes from its own `geom_type` column, not which file it's in.
struct OrderedGeomCursor {
    reader: Option<BufReader<File>>,
    schema: Vec<BinaryFieldType>,
    /// The next record read but not yet known to belong to the element being asked about — held
    /// across calls so a mismatch (this element has no entry) doesn't lose the record, which
    /// belongs to some later element.
    pending: Option<(i64, GeomValue)>,
    /// The most recently matched record, kept so a repeated `get` for the *same* `osm_id` (a
    /// side-split topic emits several tag rows per way, all sharing one geometry) reuses it instead
    /// of re-consuming the (already-advanced-past) cursor.
    last: Option<(i64, GeomValue)>,
}

impl OrderedGeomCursor {
    /// `None` if `path` doesn't exist — this topic declared no geometry output for this kind.
    fn open(path: &Path) -> anyhow::Result<Option<Self>> {
        if !path.exists() {
            return Ok(None);
        }
        let mut reader = BufReader::new(File::open(path)?);
        read_binary_header(&mut reader)?;
        Ok(Some(OrderedGeomCursor { reader: Some(reader), schema: geom_binary_schema(), pending: None, last: None }))
    }

    fn read_one(&mut self) -> anyhow::Result<Option<(i64, GeomValue)>> {
        loop {
            let Some(reader) = self.reader.as_mut() else { return Ok(None) };
            let Some(fields) = read_binary_row(reader, &self.schema)? else {
                self.reader = None; // exhausted — stop trying to read further
                return Ok(None);
            };
            let row = GeomRow::from_binary_fields(fields)?;
            if row.geom_ewkb.is_empty() {
                continue; // a way whose shape degenerated to nothing (see `WayGeometry`'s own doc)
            }
            let value = GeomValue::decode(row.geom_type, &row.geom_ewkb)?;
            return Ok(Some((row.osm_id, value)));
        }
    }

    /// This element's geometry, if the file has one — `None` otherwise. Must be called with
    /// `osm_id` in the same relative order `{table}.bin`'s same-kind rows come in (repeats for the
    /// same element, from side-split duplicates, are fine).
    fn get(&mut self, osm_id: i64) -> anyhow::Result<Option<&GeomValue>> {
        if let Some((id, _)) = &self.last {
            if *id == osm_id {
                return Ok(self.last.as_ref().map(|(_, g)| g));
            }
        }
        if self.pending.is_none() {
            self.pending = self.read_one()?;
        }
        match &self.pending {
            Some((id, _)) if *id == osm_id => {
                self.last = self.pending.take();
                Ok(self.last.as_ref().map(|(_, g)| g))
            }
            _ => Ok(None),
        }
    }
}

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
fn push_graph_fallback_features(features: &mut Vec<Value>, segments: &[&EdgeGeom], properties: &Map<String, Value>) {
    for segment in segments {
        let mut properties = properties.clone();
        properties.insert("seg_idx".to_owned(), json!(segment.seg_idx));
        features.push(json!({
            "type": "Feature",
            "geometry": { "type": "LineString", "coordinates": segment.coordinates },
            "properties": properties,
        }));
    }
}

/// Peek the next binary row's field count without consuming it — lets the tag-row reader infer
/// whether this table emits the `id` column (6 vs 7 fields, matching `tag_columns`' `emits_id`
/// conditional) straight from the data instead of needing that boolean threaded in from the caller
/// (there's no header line to read it from, unlike the old CSV path's `TagCols::from_header`).
fn peek_tag_field_count(reader: &mut BufReader<File>) -> anyhow::Result<i16> {
    let buf = reader.fill_buf()?;
    anyhow::ensure!(buf.len() >= 2, "empty or truncated tag binary file");
    Ok(i16::from_be_bytes([buf[0], buf[1]]))
}

/// Reads `{table}.bin` plus whichever of this topic's own geometry tables exist
/// (`{table}_node_geom`/`{table}_way_geom`/`{table}_relation_geom`, or the shared `edges.bin` for a
/// topic that declared `"graph": { "way": true }` without its own way geometry table) and collects
/// this topic's GeoJSON `Feature`s, cut points interleaved in with `properties.kind` set to
/// `"cut"`/`"endpoint"`. Shared by both sink framings — see the module doc.
fn build_features(
    out_dir: &Path,
    table: &str,
    edges: &[(i64, EdgeGeom)],
    edges_by_way: &HashMap<i64, Vec<&EdgeGeom>>,
    relation_members: &HashMap<i64, Vec<i64>>,
) -> anyhow::Result<Vec<Value>> {
    let mut node_geom = OrderedGeomCursor::open(&out_dir.join(format!("{table}_node_geom.bin")))?;
    let mut way_geom = OrderedGeomCursor::open(&out_dir.join(format!("{table}_way_geom.bin")))?;
    let mut relation_geom = OrderedGeomCursor::open(&out_dir.join(format!("{table}_relation_geom.bin")))?;
    let mut edge_cursor = EdgeCursor::new(edges);

    let mut reader = BufReader::new(File::open(out_dir.join(format!("{table}.bin")))?);
    read_binary_header(&mut reader)?;
    let emits_id = peek_tag_field_count(&mut reader)? == 7;
    let schema = tag_binary_schema(emits_id);
    let mut features = Vec::new();

    while let Some(fields) = read_binary_row(&mut reader, &schema)? {
        let row = TopicRow::from_binary_fields(fields)?;
        let osm_id = row.osm_id;

        match row.osm_type {
            "R" => {
                let geom = match relation_geom.as_mut() {
                    Some(cursor) => cursor.get(osm_id)?,
                    None => None,
                };
                if let Some(geom) = geom {
                    features.push(json!({
                        "type": "Feature",
                        "geometry": geom.to_geojson(),
                        "properties": base_properties(&row),
                    }));
                } else if let Some(way_ids) = relation_members.get(&osm_id) {
                    // Graph-shape fallback: no per-topic relation geometry table, stitch the
                    // shared graph's edges for this relation's member ways instead.
                    let segments: Vec<&EdgeGeom> =
                        way_ids.iter().filter_map(|way_id| edges_by_way.get(way_id)).flatten().copied().collect();
                    push_graph_fallback_features(&mut features, &segments, &base_properties(&row));
                }
            }
            "N" => {
                let geom = match node_geom.as_mut() {
                    Some(cursor) => cursor.get(osm_id)?,
                    None => None,
                };
                let Some(GeomValue::Point(p)) = geom else { continue };
                features.push(json!({
                    "type": "Feature",
                    "geometry": { "type": "Point", "coordinates": p },
                    "properties": base_properties(&row),
                }));
            }
            _ => {
                let geom = match way_geom.as_mut() {
                    Some(cursor) => cursor.get(osm_id)?,
                    None => None,
                };
                if let Some(geom) = geom {
                    features.push(json!({
                        "type": "Feature",
                        "geometry": geom.to_geojson(),
                        "properties": base_properties(&row),
                    }));
                } else {
                    // Graph-shape fallback (see module doc): split into intersection segments,
                    // surfacing the shared node between consecutive segments as a cut point.
                    let segments = edge_cursor.get_all(osm_id);
                    if !segments.is_empty() {
                        let properties = base_properties(&row);
                        push_graph_fallback_features(&mut features, &segments, &properties);
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
    }

    Ok(features)
}

/// Reads each of `tables`' binary-staged output (see `build_features`) and writes
/// `{table}.geojsonseq` alongside them: one GeoJSON `Feature` object per line (RFC 8142).
pub fn write_geojsonseq(out_dir: &Path, tables: &[String]) -> anyhow::Result<()> {
    let edges = read_edges(&out_dir.join("edges.bin"))?;
    let edges_by_way = group_edges_by_way(&edges);
    let relation_members = read_relation_members(&out_dir.join("relation_members.bin"))?;

    for table in tables {
        let features = build_features(out_dir, table, &edges, &edges_by_way, &relation_members)?;
        let mut writer = std::io::BufWriter::new(File::create(out_dir.join(format!("{table}.geojsonseq")))?);
        for feature in &features {
            serde_json::to_writer(&mut writer, feature)?;
            writer.write_all(b"\n")?;
        }
    }
    Ok(())
}

/// Reads each of `tables`' binary-staged output (see `build_features`) and writes `{table}.geojson`
/// alongside them: a single `{"type": "FeatureCollection", "features": [...]}` object — simpler to
/// consume whole than `geojsonseq`, at the cost of buffering the whole topic in memory.
pub fn write_geojson(out_dir: &Path, tables: &[String]) -> anyhow::Result<()> {
    let edges = read_edges(&out_dir.join("edges.bin"))?;
    let edges_by_way = group_edges_by_way(&edges);
    let relation_members = read_relation_members(&out_dir.join("relation_members.bin"))?;

    for table in tables {
        let features = build_features(out_dir, table, &edges, &edges_by_way, &relation_members)?;
        let feature_collection = json!({ "type": "FeatureCollection", "features": features });
        std::fs::write(out_dir.join(format!("{table}.geojson")), serde_json::to_vec(&feature_collection)?)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg(seg_idx: usize) -> EdgeGeom {
        EdgeGeom { seg_idx, coordinates: vec![[0.0, 0.0], [1.0, 1.0]] }
    }

    #[test]
    fn get_all_returns_contiguous_run_and_advances_past_it() {
        // way 1 has two segments, way 2 has one, way 3 has none in `edges` at all.
        let edges = vec![(1, seg(0)), (1, seg(1)), (2, seg(0))];
        let mut cursor = EdgeCursor::new(&edges);

        let way1 = cursor.get_all(1);
        assert_eq!(way1.len(), 2);
        assert_eq!(way1[0].seg_idx, 0);
        assert_eq!(way1[1].seg_idx, 1);

        // way 3 isn't in `edges` at all — no match, no advance, next call for way 2 still works.
        assert!(cursor.get_all(3).is_empty());

        let way2 = cursor.get_all(2);
        assert_eq!(way2.len(), 1);
        assert_eq!(way2[0].seg_idx, 0);

        // repeated call for the same way reuses `last` rather than re-scanning past it.
        assert_eq!(cursor.get_all(2).len(), 1);

        // a genuinely new id past the end of `edges` finds nothing.
        assert!(cursor.get_all(4).is_empty());
    }

    #[test]
    fn get_all_reuses_last_match_for_repeated_calls() {
        // A side-split topic asks for the same way's segments more than once in a row.
        let edges = vec![(1, seg(0)), (1, seg(1)), (2, seg(0))];
        let mut cursor = EdgeCursor::new(&edges);

        let first = cursor.get_all(1);
        assert_eq!(first.len(), 2);
        let repeat = cursor.get_all(1);
        assert_eq!(repeat.len(), 2);

        // cursor only advanced once — way 2 is still reachable after the repeat.
        let way2 = cursor.get_all(2);
        assert_eq!(way2.len(), 1);
    }

    #[test]
    fn get_all_on_empty_edges_always_returns_empty() {
        let edges: Vec<(i64, EdgeGeom)> = Vec::new();
        let mut cursor = EdgeCursor::new(&edges);
        assert!(cursor.get_all(1).is_empty());
        assert!(cursor.get_all(2).is_empty());
    }

    #[test]
    fn group_edges_by_way_preserves_seg_idx_order() {
        let edges = vec![(1, seg(0)), (1, seg(1)), (2, seg(0))];
        let grouped = group_edges_by_way(&edges);
        let way1 = grouped.get(&1).unwrap();
        assert_eq!(way1.len(), 2);
        assert_eq!(way1[0].seg_idx, 0);
        assert_eq!(way1[1].seg_idx, 1);
        assert_eq!(grouped.get(&2).unwrap().len(), 1);
        assert!(grouped.get(&3).is_none());
    }
}
