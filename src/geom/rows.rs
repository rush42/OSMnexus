//! Geometry-specific output row types and their CSV column layouts. `TopicRow`/`MemberRow` (not
//! geometry — the tag/link tables) stay in `output::rows`, which also owns the shared `CsvRow`
//! trait/`write_csv_row` both modules' row types implement/use.

use crate::output::rows::CsvRow;

/// Column lists shared by the COPY statement and the CSV header line (no spaces → valid as both).
/// The field order here **must** match each row type's `csv_fields` implementation below.
pub const GEOM_COLUMNS: &str = "osm_id,seg_idx,start_id,end_id,geom,length_m,total_length_m,cost,reverse_cost";
pub const WAY_GEOM_COLUMNS: &str = "osm_id,geom,length_m";
pub const NODE_COLUMNS: &str = "id,osm_id,geom";
pub const POINT_COLUMNS: &str = "osm_id,geom";
pub const POLYGON_COLUMNS: &str = "osm_id,geom";

/// A single graph-edge row: one per intersection sub-linestring of a way (`edges` table, always
/// emitted — this *is* the extracted graph). Shared across all topics and all side objects of a way
/// (side-split is a tag-only operation), so keyed on `osm_id`. `start_id`/`end_id` are internal graph
/// vertex ids (see `assign_node_ids`), not raw OSM node ids — they join `nodes.id`. `cost`/
/// `reverse_cost` are always equal to `length_m` — see `create_edge_table_sql`'s doc comment for why.
pub struct GeomRow {
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
            self.cost.to_string(),
            self.reverse_cost.to_string(),
        ])
    }
}

/// A whole linestring row: one `{table}_geom` (way) or `{table}_relation_geom` (relation) row per
/// kept element declaring `"geometry": { "<kind>": ["line"] }` (see `TopicRunner::wants`).
/// `Clone` since the same row can fan out to however many topics want it.
#[derive(Clone)]
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
    fn csv_fields(&self) -> anyhow::Result<Vec<String>> {
        Ok(vec![self.id.to_string(), self.osm_id.to_string(), hex::encode(&self.geom_ewkb)])
    }
}

/// A single-point row: a node's own coordinate, a way's centroid, or a relation's centroid —
/// routed (see `main.rs`) to every topic that declares `"geometry": { "<kind>": ["point"] }` and
/// kept the element. `Clone` since the same row can fan out to however many topics want it.
#[derive(Clone)]
pub struct PointRow {
    pub osm_id: i64,
    pub geom_ewkb: Vec<u8>,
}

impl CsvRow for PointRow {
    /// CSV field order matches `POINT_COLUMNS`.
    fn csv_fields(&self) -> anyhow::Result<Vec<String>> {
        Ok(vec![self.osm_id.to_string(), hex::encode(&self.geom_ewkb)])
    }
}

/// A polygon row: a closed way's own ring, or a relation's assembled multipolygon (exterior +
/// holes, see `geom::relation::assemble_rings`). Routed (see `main.rs`) to every topic that
/// declares `"geometry": { "<kind>": ["polygon"] }` and kept the element. `Clone` since the same
/// row can fan out to however many topics want it.
#[derive(Clone)]
pub struct PolygonRow {
    pub osm_id: i64,
    pub geom_ewkb: Vec<u8>,
}

impl CsvRow for PolygonRow {
    /// CSV field order matches `POLYGON_COLUMNS`.
    fn csv_fields(&self) -> anyhow::Result<Vec<String>> {
        Ok(vec![self.osm_id.to_string(), hex::encode(&self.geom_ewkb)])
    }
}
