//! Geometry-specific output row types and their per-sink encodings. `TopicRow`/`MemberRow` (not
//! geometry — the tag/link tables) stay in `output::rows`, which also owns the `BinaryRow`/`CsvRow`
//! traits both modules' row types implement and the doc explaining why each sink encodes the struct
//! itself rather than sharing one canonical encoding.

use crate::output::rows::{BinaryField, BinaryRow, CsvRow};
use crate::output::stage::{
    put_bytes, put_f64, put_i64, put_opt_f64, put_str, put_u32, StageCursor, StageDecode, StageRow,
};

/// Column lists shared by the COPY statement and the CSV header line (no spaces → valid as both).
/// The field order here **must** match each row type's `binary_fields`/`csv_fields` implementations
/// below.
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

impl CsvRow for EdgeRow {
    fn csv_fields(&self, out: &mut Vec<String>) {
        out.push(self.osm_id.to_string());
        out.push(self.seg_idx.to_string());
        out.push(self.start_id.to_string());
        out.push(self.end_id.to_string());
        out.push(hex::encode(&self.geom_ewkb));
        out.push(self.length_m.to_string());
        out.push(self.total_length_m.to_string());
        out.push(self.cost.to_string());
        out.push(self.reverse_cost.to_string());
    }
}

impl StageRow for EdgeRow {
    fn stage_encode(&self, buf: &mut Vec<u8>) {
        put_i64(buf, self.osm_id);
        put_u32(buf, self.seg_idx as u32);
        put_i64(buf, self.start_id);
        put_i64(buf, self.end_id);
        put_bytes(buf, &self.geom_ewkb);
        put_f64(buf, self.length_m);
        put_f64(buf, self.total_length_m);
        put_f64(buf, self.cost);
        put_f64(buf, self.reverse_cost);
    }
}

impl StageDecode for EdgeRow {
    type Ctx = ();

    fn stage_decode(cur: &mut StageCursor<'_>, _: &mut ()) -> anyhow::Result<Self> {
        Ok(EdgeRow {
            osm_id: cur.i64()?,
            seg_idx: cur.u32()? as usize,
            start_id: cur.i64()?,
            end_id: cur.i64()?,
            geom_ewkb: cur.bytes()?.to_vec(),
            length_m: cur.f64()?,
            total_length_m: cur.f64()?,
            cost: cur.f64()?,
            reverse_cost: cur.f64()?,
        })
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
/// out — decoding maps a staged string back onto one of those instead of leaking an owned `String`
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

impl CsvRow for GeomRow {
    fn csv_fields(&self, out: &mut Vec<String>) {
        out.push(self.osm_id.to_string());
        out.push(self.geom_type.to_owned());
        out.push(hex::encode(&self.geom_ewkb));
        out.push(self.length_m.map(|v| v.to_string()).unwrap_or_default());
    }
}

impl StageRow for GeomRow {
    fn stage_encode(&self, buf: &mut Vec<u8>) {
        put_i64(buf, self.osm_id);
        put_str(buf, self.geom_type);
        put_bytes(buf, &self.geom_ewkb);
        put_opt_f64(buf, self.length_m);
    }
}

impl StageDecode for GeomRow {
    type Ctx = ();

    fn stage_decode(cur: &mut StageCursor<'_>, _: &mut ()) -> anyhow::Result<Self> {
        let osm_id = cur.i64()?;
        let geom_type = geom_type_from_str(cur.str()?)?;
        Ok(GeomRow { osm_id, geom_type, geom_ewkb: cur.bytes()?.to_vec(), length_m: cur.opt_f64()? })
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

impl CsvRow for NodeRow {
    fn csv_fields(&self, out: &mut Vec<String>) {
        out.push(self.id.to_string());
        out.push(self.osm_id.to_string());
        out.push(hex::encode(&self.geom_ewkb));
    }
}

impl StageRow for NodeRow {
    fn stage_encode(&self, buf: &mut Vec<u8>) {
        put_i64(buf, self.id);
        put_i64(buf, self.osm_id);
        put_bytes(buf, &self.geom_ewkb);
    }
}

/// No `StageDecode`: `nodes` is a terminal table. `pg`/`csv` are one-pass writers that never re-read
/// their own output, and the file backends' post-run join reads tag/geometry/edges/member tables
/// only — nothing ever decodes a staged `NodeRow`.
#[cfg(test)]
mod tests {
    use super::*;

    fn stage_round_trip<T: StageRow + StageDecode>(row: &T) -> T {
        let mut buf = Vec::new();
        row.stage_encode(&mut buf);
        let mut cur = StageCursor::new(&buf);
        T::stage_decode(&mut cur, &mut <T as StageDecode>::Ctx::default()).unwrap()
    }

    fn csv_of<T: CsvRow>(row: &T) -> Vec<String> {
        let mut out = Vec::new();
        row.csv_fields(&mut out);
        out
    }

    fn edge_row() -> EdgeRow {
        EdgeRow {
            osm_id: 1,
            seg_idx: 2,
            start_id: 10,
            end_id: 20,
            geom_ewkb: vec![1, 2, 3, 4],
            length_m: 12.5,
            total_length_m: 30.0,
            cost: 12.5,
            reverse_cost: 12.5,
        }
    }

    #[test]
    fn edge_row_stage_round_trips() {
        let decoded = stage_round_trip(&edge_row());
        assert_eq!(decoded.osm_id, 1);
        assert_eq!(decoded.seg_idx, 2);
        assert_eq!(decoded.start_id, 10);
        assert_eq!(decoded.end_id, 20);
        assert_eq!(decoded.geom_ewkb, vec![1, 2, 3, 4]);
        assert_eq!(decoded.length_m, 12.5);
        assert_eq!(decoded.total_length_m, 30.0);
        assert_eq!(decoded.cost, 12.5);
        assert_eq!(decoded.reverse_cost, 12.5);
    }

    #[test]
    fn geom_row_stage_round_trips_with_and_without_length() {
        let row = GeomRow { osm_id: 5, geom_type: "LineString", geom_ewkb: vec![9, 8, 7], length_m: Some(4.0) };
        let decoded = stage_round_trip(&row);
        assert_eq!(decoded.osm_id, 5);
        assert_eq!(decoded.geom_type, "LineString");
        assert_eq!(decoded.geom_ewkb, vec![9, 8, 7]);
        assert_eq!(decoded.length_m, Some(4.0));

        let row = GeomRow { osm_id: 6, geom_type: "Point", geom_ewkb: vec![1], length_m: None };
        assert_eq!(stage_round_trip(&row).length_m, None);
    }

    #[test]
    fn csv_fields_match_the_column_lists() {
        assert_eq!(csv_of(&edge_row()).len(), EDGE_COLUMNS.split(',').count());
        assert_eq!(
            csv_of(&GeomRow { osm_id: 5, geom_type: "Point", geom_ewkb: vec![0xab], length_m: None }),
            vec!["5".to_owned(), "Point".to_owned(), "ab".to_owned(), String::new()],
        );
        assert_eq!(
            csv_of(&NodeRow { id: 3, osm_id: 77, geom_ewkb: vec![0x01, 0xff] }),
            vec!["3".to_owned(), "77".to_owned(), "01ff".to_owned()],
        );
    }
}
