//! Output row types and their CSV serialization — the single source of truth for the tag/geom
//! column order, shared by the Postgres COPY path and the CSV file path (see `output::writers`).

use serde_json::{Map, Value};

use crate::output::types::OsmMeta;

/// Column lists shared by the COPY statement and the CSV header line (no spaces → valid as both).
/// The field order here **must** match each row type's `csv_fields` implementation below.
pub const TAG_COLUMNS: &str = "osm_id,osm_type,id,osm,derived,private,meta,minzoom";
pub const GEOM_COLUMNS: &str = "osm_id,seg_idx,start_id,end_id,geom,length_m,total_length_m";
pub const MEMBER_COLUMNS: &str = "relation_osm_id,way_osm_id";
pub const WAY_GEOM_COLUMNS: &str = "osm_id,geom,length_m";
pub const NODE_GEOM_COLUMNS: &str = "osm_id,geom";

/// A row that can be serialized into an ordered list of CSV fields. Implemented by both output row
/// types so the writers (`output::writers`) can be generic over the row type.
pub trait CsvRow {
    /// The CSV fields in `*_COLUMNS` order. Fallible because tag rows serialize JSON maps.
    fn csv_fields(&self) -> anyhow::Result<Vec<String>>;
}

/// A single tag row produced by the topic engine — one per (way, side, prefix), independent of
/// how the geometry is later cut. Geometry lives in the paired geom table (see `GeomRow`), joined
/// on `osm_id` at tile-materialization time.
/// All four data columns are runtime JSON maps — no per-topic typed structs needed.
pub struct TopicRow {
    pub osm_id: i64,
    pub osm_type: &'static str,
    pub id: String,
    pub osm: Map<String, Value>,
    /// Merged sanitizer + deriver outputs (the former `sanitized` ∪ `derived` columns).
    pub derived: Map<String, Value>,
    pub private: Map<String, Value>,
    pub meta: OsmMeta,
    pub minzoom: i32,
}

impl CsvRow for TopicRow {
    /// CSV field order matches `TAG_COLUMNS`.
    fn csv_fields(&self) -> anyhow::Result<Vec<String>> {
        Ok(vec![
            self.osm_id.to_string(),
            self.osm_type.to_owned(),
            self.id.clone(),
            serde_json::to_string(&self.osm)?,
            serde_json::to_string(&self.derived)?,
            serde_json::to_string(&self.private)?,
            serde_json::to_string(&self.meta)?,
            self.minzoom.to_string(),
        ])
    }
}

/// A single graph-edge row: one per intersection sub-linestring of a way (`edges` table, always
/// emitted — this *is* the extracted graph). Shared across all topics and all side objects of a way
/// (side-split is a tag-only operation), so keyed on `osm_id`.
pub struct GeomRow {
    pub osm_id: i64,
    pub seg_idx: usize,
    pub start_id: i64,
    pub end_id: i64,
    pub geom_ewkb: Vec<u8>,
    pub length_m: f64,
    pub total_length_m: f64,
}

impl CsvRow for GeomRow {
    /// CSV field order matches `GEOM_COLUMNS`.
    fn csv_fields(&self) -> anyhow::Result<Vec<String>> {
        Ok(vec![
            self.osm_id.to_string(),
            self.seg_idx.to_string(),
            self.start_id.to_string(),
            self.end_id.to_string(),
            hex::encode(&self.geom_ewkb),
            self.length_m.to_string(),
            self.total_length_m.to_string(),
        ])
    }
}

/// A whole-way linestring row (`way_geometries` table), emitted only with `--emit-way-geometries`.
pub struct WayGeomRow {
    pub osm_id: i64,
    pub geom_ewkb: Vec<u8>,
    pub length_m: f64,
}

impl CsvRow for WayGeomRow {
    /// CSV field order matches `WAY_GEOM_COLUMNS`.
    fn csv_fields(&self) -> anyhow::Result<Vec<String>> {
        Ok(vec![self.osm_id.to_string(), hex::encode(&self.geom_ewkb), self.length_m.to_string()])
    }
}

/// A node point-geometry row (`node_geometries` table), emitted only with `--emit-node-geometries`.
pub struct NodeGeomRow {
    pub osm_id: i64,
    pub geom_ewkb: Vec<u8>,
}

impl CsvRow for NodeGeomRow {
    /// CSV field order matches `NODE_GEOM_COLUMNS`.
    fn csv_fields(&self) -> anyhow::Result<Vec<String>> {
        Ok(vec![self.osm_id.to_string(), hex::encode(&self.geom_ewkb)])
    }
}

/// A relation → member-way link row, emitted for every way member of a kept relation. Lets a
/// relation's tag row be joined downstream to the geometries of its member ways (relations have no
/// materialized geometry of their own).
pub struct MemberRow {
    pub relation_osm_id: i64,
    pub way_osm_id: i64,
}

impl CsvRow for MemberRow {
    /// CSV field order matches `MEMBER_COLUMNS`.
    fn csv_fields(&self) -> anyhow::Result<Vec<String>> {
        Ok(vec![self.relation_osm_id.to_string(), self.way_osm_id.to_string()])
    }
}

/// Append one CSV record (RFC-4180-ish quoting) to `buf`. Shared by the COPY sink writers and the
/// CSV file writers.
pub fn write_csv_row(buf: &mut Vec<u8>, fields: &[String]) {
    for (i, field) in fields.iter().enumerate() {
        if i > 0 { buf.push(b','); }
        let needs_quoting = field.contains('"') || field.contains(',')
            || field.contains('\n') || field.contains('\\');
        if needs_quoting {
            buf.push(b'"');
            for ch in field.chars() {
                if ch == '"' { buf.extend_from_slice(b"\"\""); }
                else {
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
