//! Geometry-specific output row types and their CSV column layouts. `TopicRow`/`MemberRow` (not
//! geometry — the tag/link tables) stay in `output::rows`, which also owns the shared `CsvRow`
//! trait/`write_csv_row` both modules' row types implement/use.

use crate::output::rows::{BinaryField, BinaryRow, CsvRow};

/// Column lists shared by the COPY statement and the CSV header line (no spaces → valid as both).
/// The field order here **must** match each row type's `csv_fields` implementation below.
pub const EDGE_COLUMNS: &str = "osm_id,seg_idx,start_id,end_id,geom,length_m,total_length_m,cost,reverse_cost";
pub const NODE_COLUMNS: &str = "id,osm_id,geom";
/// Column list for a per-kind geometry table (`{table}_node_geom`/`{table}_way_geom`/
/// `{table}_relation_geom}` — see `GeomRow`'s own doc).
pub const GEOM_COLUMNS: &str = "osm_id,geom_type,geom,length_m";

/// A single graph-edge row: one per intersection sub-linestring of a way (`edges` table, always
/// emitted — this *is* the extracted graph). Shared across all topics and all side objects of a way
/// (side-split is a tag-only operation), so keyed on `osm_id`. `start_id`/`end_id` are internal graph
/// vertex ids (see `assign_node_ids`), not raw OSM node ids — they join `nodes.id`. `cost`/
/// `reverse_cost` are always equal to `length_m` — see `create_edge_table_sql`'s doc comment for why.
pub struct EdgeRow {
    pub osm_id: i64,
    pub seg_idx: usize,
    pub start_id: i64,
    pub end_id: i64,
    pub geom_ewkb: Vec<u8>,
    pub length_m: f64,
    pub total_length_m: f64,
    pub cost: f64,
    pub reverse_cost: f64,
}

impl CsvRow for EdgeRow {
    /// CSV field order matches `EDGE_COLUMNS`.
    fn csv_fields(self) -> anyhow::Result<Vec<String>> {
        Ok(vec![
            self.osm_id.to_string(),
            self.seg_idx.to_string(),
            self.start_id.to_string(),
            self.end_id.to_string(),
            hex::encode(&self.geom_ewkb),
            self.length_m.to_string(),
            self.total_length_m.to_string(),
            self.cost.to_string(),
            self.reverse_cost.to_string(),
        ])
    }
}

impl BinaryRow for EdgeRow {
    /// Field order matches `EDGE_COLUMNS`; `seg_idx` (`integer` column) is cast from `usize` —
    /// always small (segments per way), never near `i32::MAX`.
    fn binary_fields(self) -> anyhow::Result<Vec<BinaryField>> {
        Ok(vec![
            BinaryField::Int8(self.osm_id),
            BinaryField::Int4(self.seg_idx as i32),
            BinaryField::Int8(self.start_id),
            BinaryField::Int8(self.end_id),
            BinaryField::Bytea(self.geom_ewkb),
            BinaryField::Float8(self.length_m),
            BinaryField::Float8(self.total_length_m),
            BinaryField::Float8(self.cost),
            BinaryField::Float8(self.reverse_cost),
        ])
    }
}

/// One row of a per-kind geometry table (`{table}_node_geom`/`{table}_way_geom`/
/// `{table}_relation_geom`) — a node's own coordinate, a way's/relation's whole linestring or
/// closed-ring polygon, or a way's/relation's centroid, per whichever single shape that topic
/// declared for that kind (`GeometryOutputSpec` — at most one now, see its own doc). `geom_type`
/// (`"Point"`/`"LineString"`/`"MultiLineString"`/`"Polygon"`) self-describes the row the same way
/// `TopicRow::osm_type` self-describes a tag row's element kind, so one table can hold every shape
/// a kind can produce instead of needing a separate table per (kind, shape) pair.
/// `MultiLineString` is relation-line only (see `geom::builders::build_relation_line_row`'s own
/// doc: a relation's member ways chained by shared endpoint frequently assemble into several
/// disconnected runs, carried as one row rather than splitting into several). `length_m` is `None`
/// for `Point`/`Polygon` — only a line has a length. `Clone` since the same row can fan out to
/// however many topics want it.
#[derive(Clone)]
pub struct GeomRow {
    pub osm_id: i64,
    pub geom_type: &'static str,
    pub geom_ewkb: Vec<u8>,
    pub length_m: Option<f64>,
}

impl CsvRow for GeomRow {
    /// CSV field order matches `GEOM_COLUMNS`. Empty field = NULL, same convention `TopicRow`'s
    /// `category` uses.
    fn csv_fields(self) -> anyhow::Result<Vec<String>> {
        Ok(vec![
            self.osm_id.to_string(),
            self.geom_type.to_owned(),
            hex::encode(&self.geom_ewkb),
            self.length_m.map_or_else(String::new, |l| l.to_string()),
        ])
    }
}

impl BinaryRow for GeomRow {
    /// Field order matches `GEOM_COLUMNS`.
    fn binary_fields(self) -> anyhow::Result<Vec<BinaryField>> {
        Ok(vec![
            BinaryField::Int8(self.osm_id),
            BinaryField::Text(self.geom_type.to_owned()),
            BinaryField::Bytea(self.geom_ewkb),
            self.length_m.map_or(BinaryField::Null, BinaryField::Float8),
        ])
    }
}

/// A graph-vertex row (`nodes` table, always emitted): every node referenced as a `start_id`/
/// `end_id` in `edges` — shared between ≥2 ways, a way endpoint, or forced by a node classifier. `id`
/// is the internal sequential id `edges.start_id`/`end_id` join against; `osm_id` is the original OSM
/// node id, kept for lookups/debugging. Was `--emit-node-geometries`'s `node_geometries` table —
/// that flag is gone since this mapping is now load-bearing (pgRouting-style `source`/`target`), not
/// just an optional debugging aid.
pub struct NodeRow {
    pub id: i64,
    pub osm_id: i64,
    pub geom_ewkb: Vec<u8>,
}

impl CsvRow for NodeRow {
    /// CSV field order matches `NODE_COLUMNS`.
    fn csv_fields(self) -> anyhow::Result<Vec<String>> {
        Ok(vec![self.id.to_string(), self.osm_id.to_string(), hex::encode(&self.geom_ewkb)])
    }
}

impl BinaryRow for NodeRow {
    /// Field order matches `NODE_COLUMNS`.
    fn binary_fields(self) -> anyhow::Result<Vec<BinaryField>> {
        Ok(vec![
            BinaryField::Int8(self.id),
            BinaryField::Int8(self.osm_id),
            BinaryField::Bytea(self.geom_ewkb),
        ])
    }
}

