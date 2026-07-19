//! Topic-independent geometry table rows: graph edges (`GeomRow`), whole-way linestrings
//! (`WayGeomRow`), and graph vertices (`NodeRow`). Same for every topic and every side object —
//! computed once per way/node, separate from `pipeline`'s per-topic, per-object tag/field rows.

use crate::osm::types::OsmWay;
use crate::output::{
    geometry::{
        centroid_of_line, haversine_length_m, point_to_ewkb, polygon_to_ewkb, project_line, project_polygon,
        project_ring, to_ewkb, wgs84_to_3857,
    },
    rows::{GeomRow, NodeRow, PointRow, PolygonRow, WayGeomRow},
};
use rustc_hash::FxHashMap;

/// Build the graph-edge rows for a way: one `GeomRow` per consecutive `cut_points` pair (the
/// sub-linestring `geom.0[s..=e]`, inclusive so the shared node joins both neighbours), with
/// per-segment `start_id`/`end_id`/`length_m`. A way with no interior cut-points yields a single
/// edge spanning the whole way. Topic-independent (same for every topic and every side object), so
/// computed once per way and written to the shared `edges` table.
///
/// `node_ids` is the `osm node id -> internal id` map built once by `assign_node_ids` before the
/// geometry pass starts; every cut-point node is guaranteed a spot in it (shared, endpoint, or
/// selected), so `start_id`/`end_id` are always resolvable.
pub fn build_geom_rows(
    way: &OsmWay,
    geom: &geo::LineString<f64>,
    length_m: f64,
    node_ids: &FxHashMap<i64, i64>,
) -> Vec<GeomRow> {
    let node_id = |osm_id: i64| -> i64 {
        *node_ids.get(&osm_id).expect("cut-point node missing from internal node id map")
    };
    let first_node = way.cut_points.first().map(|c| node_id(c.1)).unwrap_or(0);
    let last_node = way.cut_points.last().map(|c| node_id(c.1)).unwrap_or(0);

    let mut rows = Vec::new();

    if way.cut_points.len() > 2 {
        for (i, w) in way.cut_points.windows(2).enumerate() {
            let (s, e) = (w[0].0 as usize, w[1].0 as usize);
            let seg_line = geo::LineString::new(geom.0[s..=e].to_vec());
            let seg_len = haversine_length_m(&way.coords[s..=e]);
            rows.push(GeomRow {
                osm_id: way.id,
                seg_idx: i,
                start_id: node_id(w[0].1),
                end_id: node_id(w[1].1),
                geom_ewkb: to_ewkb(&seg_line),
                length_m: seg_len,
                total_length_m: length_m,
                cost: seg_len,
                reverse_cost: seg_len,
            });
        }
    } else {
        // No interior intersections: the whole way is a single edge.
        rows.push(GeomRow {
            osm_id: way.id,
            seg_idx: 0,
            start_id: first_node,
            end_id: last_node,
            geom_ewkb: to_ewkb(geom),
            length_m,
            total_length_m: length_m,
            cost: length_m,
            reverse_cost: length_m,
        });
    }

    rows
}

/// Build the whole-way linestring row for a way — routed (see `main.rs`'s `build_geom_cb`) to
/// every topic that declares `"geometry": { "way": ["linestring"] }` and kept this way.
pub fn build_way_geom_row(way: &OsmWay, geom: &geo::LineString<f64>, length_m: f64) -> WayGeomRow {
    WayGeomRow { osm_id: way.id, geom_ewkb: to_ewkb(geom), length_m }
}

/// Build a `nodes` table row for a graph vertex — always emitted (see `assign_node_ids`).
pub fn build_node_row(id: i64, osm_id: i64, lon: f64, lat: f64) -> NodeRow {
    let (x, y) = wgs84_to_3857(lon, lat);
    NodeRow { id, osm_id, geom_ewkb: point_to_ewkb(x, y) }
}

/// Build a topic-level point row for a plain node — routed to every topic that kept the node and
/// declares `"geometry": { "node": ["point"] }` (see `main.rs`). Distinct from `build_node_row`,
/// which is the always-on shared graph-vertex table keyed by internal id, not `osm_id`.
pub fn build_node_point_row(osm_id: i64, lon: f64, lat: f64) -> PointRow {
    let (x, y) = wgs84_to_3857(lon, lat);
    PointRow { osm_id, geom_ewkb: point_to_ewkb(x, y) }
}

/// Build a way's centroid point row — routed to every topic that both kept this way and declares
/// `"geometry": { "way": ["point"] }` (see `main.rs`). Centroid of the way's own vertices, not an
/// area-weighted centroid (a way is a line, not yet a closed shape at this point).
pub fn build_way_point_row(way: &OsmWay, geom: &geo::LineString<f64>) -> Option<PointRow> {
    let (x, y) = centroid_of_line(geom)?;
    let _ = way; // centroid is purely geometric; kept for a consistent per-way builder signature
    Some(PointRow { osm_id: way.id, geom_ewkb: point_to_ewkb(x, y) })
}

/// Build a way's closed-ring polygon row — routed to every topic that both kept this way and
/// declares `"geometry": { "way": ["polygon"] }` (see `main.rs`). Single ring only (no holes) —
/// a way itself never has inner rings; that's a relation/multipolygon concept, not yet built here
/// (see `topic::spec::GeometrySpec`'s own doc).
pub fn build_way_polygon_row(way: &OsmWay) -> PolygonRow {
    PolygonRow { osm_id: way.id, geom_ewkb: polygon_to_ewkb(&project_ring(&way.coords)) }
}

/// Build a relation's line row from its member ways' independently re-resolved coordinate
/// sequences (see `osm::relation_geometry::resolve_relation_ways`), concatenated in member order.
/// This is a simple concatenation, not a topological line-merge (`ST_LineMerge`'s old SQL
/// behavior) — correct for the common case of an ordered route relation, but won't reorder
/// out-of-sequence or reversed member ways. `None` if fewer than 2 points end up in the
/// concatenation (e.g. every member way was missing/unresolvable).
pub fn build_relation_line_row(rel_id: i64, member_coords: &[Vec<(f64, f64)>]) -> Option<WayGeomRow> {
    let coords: Vec<(f64, f64)> = member_coords.iter().flatten().copied().collect();
    if coords.len() < 2 {
        return None;
    }
    let geom = project_line(&coords);
    let length_m = haversine_length_m(&coords);
    Some(WayGeomRow { osm_id: rel_id, geom_ewkb: to_ewkb(&geom), length_m })
}

/// Build a relation's multipolygon row from its already-chained `outer`/`inner` rings (see
/// `osm::relation_geometry::assemble_rings`). Takes the largest assembled outer ring as the
/// exterior and every inner ring as a hole of it — doesn't spatially match holes to their actual
/// enclosing shape when a relation has multiple disjoint outer rings (a true multi-outer
/// multipolygon), which is rare for buildings but real for some admin/landuse relations; that
/// case would need a point-in-ring test per hole, not implemented here. `None` if there's no outer
/// ring at all (a malformed or role-less relation).
pub fn build_relation_polygon_row(
    rel_id: i64,
    outer_rings: &[Vec<(f64, f64)>],
    inner_rings: &[Vec<(f64, f64)>],
) -> Option<PolygonRow> {
    let exterior = outer_rings.iter().max_by_key(|r| r.len())?;
    let polygon = project_polygon(exterior, inner_rings);
    Some(PolygonRow { osm_id: rel_id, geom_ewkb: polygon_to_ewkb(&polygon) })
}

/// Build a relation's centroid point row from its member ways' coordinates (all points pooled
/// together, not weighted per-way-length) — see `build_relation_line_row`'s own doc for the same
/// member-coordinate source. `None` if no member way resolved to any points.
pub fn build_relation_point_row(rel_id: i64, member_coords: &[Vec<(f64, f64)>]) -> Option<PointRow> {
    let coords: Vec<(f64, f64)> = member_coords.iter().flatten().copied().collect();
    if coords.is_empty() {
        return None;
    }
    let geom = project_line(&coords);
    let (x, y) = centroid_of_line(&geom)?;
    Some(PointRow { osm_id: rel_id, geom_ewkb: point_to_ewkb(x, y) })
}
