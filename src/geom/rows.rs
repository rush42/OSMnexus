//! Geometry-specific output row types and their binary column layouts. `TopicRow`/`MemberRow` (not
//! geometry — the tag/link tables) stay in `output::rows`, which also owns the shared
//! `BinaryField`/`BinaryRow`/`FromBinaryRow` machinery both modules' row types implement/use.

use crate::output::rows::{BinaryField, BinaryFieldType, BinaryRow, FromBinaryRow};

/// Column lists shared by the COPY statement and the CSV header line (no spaces → valid as both).
/// The field order here **must** match each row type's `binary_fields` implementation below.
pub const EDGE_COLUMNS: &str = "osm_id,seg_idx,start_id,end_id,geom,length_m,total_length_m,cost,reverse_cost";
pub const NODE_COLUMNS: &str = "id,osm_id,geom";
/// Column list for a per-kind geometry table (`{table}_node_geom`/`{table}_way_geom`/
/// `{table}_relation_geom}` — see `GeomRow`'s own doc).
pub const GEOM_COLUMNS: &str = "osm_id,geom_type,geom,length_m";

/// `EDGE_COLUMNS`'s field types, for `EdgeRow::from_binary_fields`'s caller to pass to
/// `read_binary_row`.
pub fn edge_binary_schema() -> Vec<BinaryFieldType> {
    vec![
        BinaryFieldType::Int8,
        BinaryFieldType::Int4,
        BinaryFieldType::Int8,
        BinaryFieldType::Int8,
        BinaryFieldType::Bytea,
        BinaryFieldType::Float8,
        BinaryFieldType::Float8,
        BinaryFieldType::Float8,
        BinaryFieldType::Float8,
    ]
}

/// `GEOM_COLUMNS`'s field types.
pub fn geom_binary_schema() -> Vec<BinaryFieldType> {
    vec![BinaryFieldType::Int8, BinaryFieldType::Text, BinaryFieldType::Bytea, BinaryFieldType::Float8]
}

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

impl FromBinaryRow for EdgeRow {
    fn from_binary_fields(fields: Vec<BinaryField>) -> anyhow::Result<Self> {
        anyhow::ensure!(fields.len() == 9, "unexpected edge row field count {}", fields.len());
        let mut it = fields.into_iter();
        let osm_id = match it.next() {
            Some(BinaryField::Int8(v)) => v,
            _ => anyhow::bail!("edge row: expected osm_id (Int8)"),
        };
        let seg_idx = match it.next() {
            Some(BinaryField::Int4(v)) => v as usize,
            _ => anyhow::bail!("edge row: expected seg_idx (Int4)"),
        };
        let start_id = match it.next() {
            Some(BinaryField::Int8(v)) => v,
            _ => anyhow::bail!("edge row: expected start_id (Int8)"),
        };
        let end_id = match it.next() {
            Some(BinaryField::Int8(v)) => v,
            _ => anyhow::bail!("edge row: expected end_id (Int8)"),
        };
        let geom_ewkb = match it.next() {
            Some(BinaryField::Bytea(b)) => b,
            _ => anyhow::bail!("edge row: expected geom (Bytea)"),
        };
        let length_m = match it.next() {
            Some(BinaryField::Float8(v)) => v,
            _ => anyhow::bail!("edge row: expected length_m (Float8)"),
        };
        let total_length_m = match it.next() {
            Some(BinaryField::Float8(v)) => v,
            _ => anyhow::bail!("edge row: expected total_length_m (Float8)"),
        };
        let cost = match it.next() {
            Some(BinaryField::Float8(v)) => v,
            _ => anyhow::bail!("edge row: expected cost (Float8)"),
        };
        let reverse_cost = match it.next() {
            Some(BinaryField::Float8(v)) => v,
            _ => anyhow::bail!("edge row: expected reverse_cost (Float8)"),
        };
        Ok(EdgeRow { osm_id, seg_idx, start_id, end_id, geom_ewkb, length_m, total_length_m, cost, reverse_cost })
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

/// `geom_type`'s only four possible values are the `&'static str` literals `geom::builders` hands
/// out — decoding maps a wire string back onto one of those instead of leaking an owned `String`
/// into a field declared `&'static str` (same reasoning as `output::rows`'s `osm_type_from_str`).
fn geom_type_from_str(s: &str) -> anyhow::Result<&'static str> {
    match s {
        "Point" => Ok("Point"),
        "LineString" => Ok("LineString"),
        "MultiLineString" => Ok("MultiLineString"),
        "Polygon" => Ok("Polygon"),
        other => anyhow::bail!("unknown geom_type {other:?}"),
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

impl FromBinaryRow for GeomRow {
    fn from_binary_fields(fields: Vec<BinaryField>) -> anyhow::Result<Self> {
        anyhow::ensure!(fields.len() == 4, "unexpected geom row field count {}", fields.len());
        let mut it = fields.into_iter();
        let osm_id = match it.next() {
            Some(BinaryField::Int8(v)) => v,
            _ => anyhow::bail!("geom row: expected osm_id (Int8)"),
        };
        let geom_type = match it.next() {
            Some(BinaryField::Text(s)) => geom_type_from_str(&s)?,
            _ => anyhow::bail!("geom row: expected geom_type (Text)"),
        };
        let geom_ewkb = match it.next() {
            Some(BinaryField::Bytea(b)) => b,
            _ => anyhow::bail!("geom row: expected geom (Bytea)"),
        };
        let length_m = match it.next() {
            Some(BinaryField::Float8(v)) => Some(v),
            Some(BinaryField::Null) => None,
            _ => anyhow::bail!("geom row: expected length_m (Float8 or Null)"),
        };
        Ok(GeomRow { osm_id, geom_type, geom_ewkb, length_m })
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output::rows::{read_binary_row, write_binary_row};

    fn round_trip<T: BinaryRow + FromBinaryRow>(row: T, schema: &[BinaryFieldType]) -> T {
        let fields = row.binary_fields().unwrap();
        let mut buf = Vec::new();
        write_binary_row(&mut buf, &fields);
        let mut cursor = std::io::Cursor::new(buf.as_slice());
        let decoded_fields = read_binary_row(&mut cursor, schema).unwrap().unwrap();
        T::from_binary_fields(decoded_fields).unwrap()
    }

    #[test]
    fn edge_row_round_trips() {
        let row = EdgeRow {
            osm_id: 1,
            seg_idx: 2,
            start_id: 10,
            end_id: 20,
            geom_ewkb: vec![1, 2, 3, 4],
            length_m: 12.5,
            total_length_m: 30.0,
            cost: 12.5,
            reverse_cost: 12.5,
        };
        let decoded = round_trip(row, &edge_binary_schema());
        assert_eq!(decoded.osm_id, 1);
        assert_eq!(decoded.seg_idx, 2);
        assert_eq!(decoded.start_id, 10);
        assert_eq!(decoded.end_id, 20);
        assert_eq!(decoded.geom_ewkb, vec![1, 2, 3, 4]);
        assert_eq!(decoded.length_m, 12.5);
        assert_eq!(decoded.total_length_m, 30.0);
    }

    #[test]
    fn geom_row_round_trips_with_and_without_length() {
        let row = GeomRow { osm_id: 5, geom_type: "LineString", geom_ewkb: vec![9, 8, 7], length_m: Some(4.0) };
        let decoded = round_trip(row, &geom_binary_schema());
        assert_eq!(decoded.osm_id, 5);
        assert_eq!(decoded.geom_type, "LineString");
        assert_eq!(decoded.geom_ewkb, vec![9, 8, 7]);
        assert_eq!(decoded.length_m, Some(4.0));

        let row = GeomRow { osm_id: 6, geom_type: "Point", geom_ewkb: vec![1], length_m: None };
        let decoded = round_trip(row, &geom_binary_schema());
        assert_eq!(decoded.length_m, None);
    }
}
