//! Builds GeoJSON output per topic from the CSV output `--output geojson`/`geojsonseq` shares with
//! `--output csv` (`{table}.csv` tag rows + this topic's own `{table}_node_geom`/`{table}_way_geom`/
//! `{table}_relation_geom` tables, see `db::schema`) — for local tooling (e.g. the live editor) that
//! wants one self-contained file instead of a table pair. Two sinks share the same `build_features`
//! collection step and differ only in framing: `write_geojson_from_csv` wraps them in one
//! `{table}.geojson` `FeatureCollection` (simple, but buffers the whole topic in memory and isn't
//! streamable); `write_geojsonseq_from_csv` writes one `{table}.geojsonseq` Feature object per line
//! (RFC 8142) instead.
//! Falls back to the shared `edges.csv` graph table for a topic that declared `"graph": { "way":
//! true }` without a `"geometry_output": { "way": ... }` — that's the one shape not stored per-topic
//! (see `EDGE_TABLE`'s own doc) — surfacing each way's intersection-split segments plus two kinds of
//! cut-point Feature interleaved into the same stream, each tagged `"kind"` in `properties`: `"cut"`
//! for the node shared between consecutive segments of the same way (where the graph broke it
//! apart), and `"endpoint"` for the way's own two ends (which may or may not coincide with another
//! way — the graph shape alone doesn't say). A topic with its own way geometry table has no split
//! points, so no `"kind"`-tagged features are emitted for it.

use std::collections::HashMap;
use std::io::Write;
use std::path::Path;

use serde_json::{json, Map, Value};

use crate::geom::primitives::{linestring_from_ewkb, mercator_to_wgs84, multilinestring_from_ewkb, point_from_ewkb};
use crate::geom::rows::{EDGE_COLUMNS, GEOM_COLUMNS};

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
    fn decode(geom_type: &str, geom_hex: &str) -> anyhow::Result<Self> {
        let bytes = hex::decode(geom_hex)?;
        Ok(match geom_type {
            "Point" => GeomValue::Point(lonlat_point(&bytes)?),
            "LineString" => GeomValue::Line(lonlat_coordinates(&bytes)?),
            "MultiLineString" => GeomValue::MultiLine(lonlat_multi_coordinates(&bytes)?),
            "Polygon" => GeomValue::Polygon(lonlat_coordinates(&bytes)?),
            other => anyhow::bail!("unknown geom_type {other:?} in geometry CSV"),
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

/// A `{table}_node_geom.csv`/`{table}_way_geom.csv`/`{table}_relation_geom.csv` reader consumed
/// forward, in step with `{table}.csv`'s own rows of that element kind — safe because
/// `geom::materialize` now resolves+routes a table's node/way/relation geometry in the exact same
/// blob order the select phase already routed that element's tag row in
/// (`SelectionContext::kept_way_order`/`kept_relation_order`, node points routed inline during the
/// same select-phase fold that routes the node's tag row) — so each geometry file's `osm_id`
/// sequence is a *subset* of `{table}.csv`'s same-kind row sequence, in matching relative order. No
/// random access, and no full in-memory geometry map, needed to match them up.
///
/// One geometry table per kind now (not per (kind, shape) pair — see `GeomRow`'s own doc), so this
/// single cursor type covers node/way/relation geometry alike; the shape a given row decodes to
/// comes from its own `geom_type` column, not which file it's in.
struct OrderedGeomCursor {
    reader: Option<csv::Reader<std::fs::File>>,
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
        debug_assert_eq!(GEOM_COLUMNS, "osm_id,geom_type,geom,length_m");
        Ok(Some(OrderedGeomCursor { reader: Some(csv::Reader::from_path(path)?), pending: None, last: None }))
    }

    fn read_one(&mut self) -> anyhow::Result<Option<(i64, GeomValue)>> {
        let Some(reader) = self.reader.as_mut() else { return Ok(None) };
        loop {
            let mut record = csv::StringRecord::new();
            if !reader.read_record(&mut record)? {
                self.reader = None; // exhausted — stop trying to read further
                return Ok(None);
            }
            let geom_hex = &record[2];
            if geom_hex.is_empty() {
                continue; // a way whose shape degenerated to nothing (see `WayGeometry`'s own doc)
            }
            let osm_id: i64 = record[0].parse()?;
            return Ok(Some((osm_id, GeomValue::decode(&record[1], geom_hex)?)));
        }
    }

    /// This element's geometry, if the file has one — `None` otherwise. Must be called with
    /// `osm_id` in the same relative order `{table}.csv`'s same-kind rows come in (repeats for the
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
    if let Ok(Value::Object(map)) = serde_json::from_str(json_str) {
        target.extend(map);
    }
}

/// Column positions in `{table}.csv`, resolved from its header rather than assumed.
///
/// A topic that set `"id_type": "none"` (see `TopicSpec::id_type`) has no `id` column, which shifts
/// every column after it — reading by fixed index would silently reinterpret `category` as the id,
/// and so on, producing plausible-looking nonsense rather than an error.
struct TagCols {
    id: Option<usize>,
    category: usize,
    produced: usize,
    annotations: usize,
}

impl TagCols {
    fn from_header(header: &csv::StringRecord) -> anyhow::Result<Self> {
        let at = |name: &str| -> anyhow::Result<usize> {
            header
                .iter()
                .position(|h| h == name)
                .ok_or_else(|| anyhow::anyhow!("tag CSV header missing column '{name}'"))
        };
        Ok(TagCols {
            id: header.iter().position(|h| h == "id"),
            category: at("category")?,
            produced: at("produced")?,
            annotations: at("annotations")?,
        })
    }
}

fn base_properties(record: &csv::StringRecord, osm_id: i64, cols: &TagCols) -> Map<String, Value> {
    let mut properties = Map::new();
    properties.insert("osm_id".to_owned(), json!(osm_id));
    if let Some(i) = cols.id {
        properties.insert("id".to_owned(), json!(&record[i]));
    }
    // Empty means `accept_all` (no category matched — see `TopicRow::category`); keep the column
    // out of GeoJSON properties entirely rather than emitting a misleading empty string.
    if !record[cols.category].is_empty() {
        properties.insert("category".to_owned(), json!(&record[cols.category]));
    }
    merge_properties(&mut properties, &record[cols.produced]);
    // `annotations` carries engine-attached bookkeeping (e.g. `_side`, the side-split object's
    // left/right/self side; `<output>_source`/`_confidence` provenance) rather than topic-authored
    // output — merged in alongside `produced` since it's ordinary public output too (e.g. the live
    // editor keys a line-offset expression on `_side`).
    merge_properties(&mut properties, &record[cols.annotations]);
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

/// Reads `{table}.csv` plus whichever of this topic's own geometry tables exist
/// (`{table}_node_geom`/`{table}_way_geom`/`{table}_relation_geom`, or the shared `edges.csv` for a
/// topic that declared `"graph": { "way": true }` without its own way geometry table) and collects
/// this topic's GeoJSON `Feature`s, cut points interleaved in with `properties.kind` set to
/// `"cut"`/`"endpoint"`. Shared by both sink framings — see the module doc.
fn build_features(
    out_dir: &Path,
    table: &str,
    edges: &HashMap<i64, Vec<EdgeGeom>>,
    relation_members: &HashMap<i64, Vec<i64>>,
) -> anyhow::Result<Vec<Value>> {
    let mut node_geom = OrderedGeomCursor::open(&out_dir.join(format!("{table}_node_geom.csv")))?;
    let mut way_geom = OrderedGeomCursor::open(&out_dir.join(format!("{table}_way_geom.csv")))?;
    let mut relation_geom = OrderedGeomCursor::open(&out_dir.join(format!("{table}_relation_geom.csv")))?;

    let mut reader = csv::Reader::from_path(out_dir.join(format!("{table}.csv")))?;
    let cols = TagCols::from_header(reader.headers()?)?;
    let mut features = Vec::new();

    for result in reader.records() {
        let record = result?;
        let osm_id: i64 = record[0].parse()?;
        let osm_type = &record[1];

        match osm_type {
            "R" => {
                let geom = match relation_geom.as_mut() {
                    Some(cursor) => cursor.get(osm_id)?,
                    None => None,
                };
                if let Some(geom) = geom {
                    features.push(json!({
                        "type": "Feature",
                        "geometry": geom.to_geojson(),
                        "properties": base_properties(&record, osm_id, &cols),
                    }));
                } else if let Some(way_ids) = relation_members.get(&osm_id) {
                    // Graph-shape fallback: no per-topic relation geometry table, stitch the
                    // shared graph's edges for this relation's member ways instead.
                    let segments: Vec<&EdgeGeom> =
                        way_ids.iter().filter_map(|way_id| edges.get(way_id)).flatten().collect();
                    push_graph_fallback_features(&mut features, &segments, &base_properties(&record, osm_id, &cols));
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
                    "properties": base_properties(&record, osm_id, &cols),
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
                        "properties": base_properties(&record, osm_id, &cols),
                    }));
                } else if let Some(segments) = edges.get(&osm_id) {
                    // Graph-shape fallback (see module doc): split into intersection segments,
                    // surfacing the shared node between consecutive segments as a cut point.
                    let properties = base_properties(&record, osm_id, &cols);
                    let segment_refs: Vec<&EdgeGeom> = segments.iter().collect();
                    push_graph_fallback_features(&mut features, &segment_refs, &properties);
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
