//! Topic-independent geometry row builders: turn a resolved `OsmWay` (or a relation's member-way
//! coordinates) into one of `geom::rows`'s row types — one function per shape (`point`/`line`/
//! `graph`/`polygon`) per kind. Same for every topic and every side object — computed once per
//! way/node/relation, separate from `pipeline`'s per-topic, per-object tag/field rows.

use crate::geom::primitives::{
    centroid_of_line, haversine_length_m, point_to_ewkb, polygon_to_ewkb, project_line, project_polygon,
    project_ring, to_ewkb, to_multi_ewkb, wgs84_to_3857,
};
use crate::geom::relation::assemble_rings;
use crate::geom::rows::{EdgeRow, GeomRow, NodeRow};
use crate::osm::types::OsmWay;
use rustc_hash::FxHashMap;

/// Build the graph-edge rows for a way: one `EdgeRow` per consecutive `cut_points` pair (the
/// sub-linestring `geom.0[s..=e]`, inclusive so the shared node joins both neighbours), with
/// per-segment `start_id`/`end_id`/`length_m`. A way with no interior cut-points yields a single
/// edge spanning the whole way. Topic-independent (same for every topic and every side object), so
/// computed once per way and written to the shared `edges` table.
///
/// `node_ids` is the `osm node id -> internal id` map built once by `assign_node_ids` before the
/// geometry pass starts; every cut-point node is guaranteed a spot in it (shared, endpoint, or
/// selected), so `start_id`/`end_id` are always resolvable.
pub fn build_edges(
    way: &OsmWay,
    geom: &geo::LineString<f64>,
    length_m: f64,
    node_ids: &FxHashMap<i64, i64>,
) -> Vec<EdgeRow> {
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
            rows.push(EdgeRow {
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
        rows.push(EdgeRow {
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
/// every topic that declares `"geometry_output": { "way": "line" }` and kept this way.
pub fn build_way(way: &OsmWay, geom: &geo::LineString<f64>, length_m: f64) -> GeomRow {
    GeomRow { osm_id: way.id, geom_type: "LineString", geom_ewkb: to_ewkb(geom), length_m: Some(length_m) }
}

/// Build a `nodes` table row for a graph vertex — always emitted (see `assign_node_ids`).
pub fn build_node_row(id: i64, osm_id: i64, lon: f64, lat: f64) -> NodeRow {
    let (x, y) = wgs84_to_3857(lon, lat);
    NodeRow { id, osm_id, geom_ewkb: point_to_ewkb(x, y) }
}

/// Build a topic-level point row for a plain node — routed to every topic that kept the node and
/// declares `"geometry_output": { "node": "point" }` (see `main.rs`). Distinct from
/// `build_node_row`, which is the always-on shared graph-vertex table keyed by internal id, not
/// `osm_id`.
pub fn build_node_point_row(osm_id: i64, lon: f64, lat: f64) -> GeomRow {
    let (x, y) = wgs84_to_3857(lon, lat);
    GeomRow { osm_id, geom_type: "Point", geom_ewkb: point_to_ewkb(x, y), length_m: None }
}

/// Build a way's centroid point row — routed to every topic that both kept this way and declares
/// `"geometry_output": { "way": "point" }` (see `main.rs`). Centroid of the way's own vertices, not
/// an area-weighted centroid (a way is a line, not yet a closed shape at this point).
pub fn build_way_point_row(way: &OsmWay, geom: &geo::LineString<f64>) -> Option<GeomRow> {
    let (x, y) = centroid_of_line(geom)?;
    let _ = way; // centroid is purely geometric; kept for a consistent per-way builder signature
    Some(GeomRow { osm_id: way.id, geom_type: "Point", geom_ewkb: point_to_ewkb(x, y), length_m: None })
}

/// Build a way's closed-ring polygon row — routed to every topic that both kept this way and
/// declares `"geometry_output": { "way": "polygon" }` (see `main.rs`). Single ring only (no holes)
/// — a way itself never has inner rings; that's a relation/multipolygon concept (see
/// `build_relation_polygon_row`).
pub fn build_way_polygon_row(way: &OsmWay) -> GeomRow {
    GeomRow {
        osm_id: way.id,
        geom_type: "Polygon",
        geom_ewkb: polygon_to_ewkb(&project_ring(&way.coords)),
        length_m: None,
    }
}

/// Build a relation's line row from its member ways' independently re-resolved coordinate
/// sequences (see `geom::relation::resolve_relation_ways`), chained into contiguous runs by
/// `assemble_rings` — the same topological (shared-endpoint) chaining `build_relation_polygon_row`
/// uses for rings, applied here without forcing closure. A route relation's member ways are
/// frequently listed out of geographic order (branches, editor insertion order, genuinely disjoint
/// segments), so naively concatenating them in member order draws a straight chord across every
/// such gap; chaining by shared endpoint only connects ways that are actually adjacent. The result
/// is a `MultiLineString` — one member per connected run — rather than one row per run, so a
/// multi-branch route (or a route with real mapping gaps) still emits a single relation-line row
/// instead of splitting into several. `None` if no run has at least 2 points (e.g. every member way
/// was missing/unresolvable).
pub fn build_relation_line_row(rel_id: i64, member_coords: &[Vec<(f64, f64)>]) -> Option<GeomRow> {
    let runs: Vec<Vec<(f64, f64)>> =
        assemble_rings(member_coords.to_vec()).into_iter().filter(|r| r.len() >= 2).collect();
    if runs.is_empty() {
        return None;
    }
    let length_m: f64 = runs.iter().map(|r| haversine_length_m(r)).sum();
    let lines: Vec<geo::LineString<f64>> = runs.iter().map(|r| project_line(r)).collect();
    Some(GeomRow {
        osm_id: rel_id,
        geom_type: "MultiLineString",
        geom_ewkb: to_multi_ewkb(&lines),
        length_m: Some(length_m),
    })
}

/// Build a relation's multipolygon row from its already-chained `outer`/`inner` rings (see
/// `geom::relation::assemble_rings`). Takes the largest assembled outer ring as the exterior and
/// every inner ring as a hole of it — doesn't spatially match holes to their actual enclosing
/// shape when a relation has multiple disjoint outer rings (a true multi-outer multipolygon),
/// which is rare for buildings but real for some admin/landuse relations; that case would need a
/// point-in-ring test per hole, not implemented here. `None` if there's no outer ring at all (a
/// malformed or role-less relation).
pub fn build_relation_polygon_row(
    rel_id: i64,
    outer_rings: &[Vec<(f64, f64)>],
    inner_rings: &[Vec<(f64, f64)>],
) -> Option<GeomRow> {
    let exterior = outer_rings.iter().max_by_key(|r| r.len())?;
    let polygon = project_polygon(exterior, inner_rings);
    Some(GeomRow { osm_id: rel_id, geom_type: "Polygon", geom_ewkb: polygon_to_ewkb(&polygon), length_m: None })
}

/// Build a relation's centroid point row from its member ways' coordinates (all points pooled
/// together, not weighted per-way-length) — see `build_relation_line_row`'s own doc for the same
/// member-coordinate source. `None` if no member way resolved to any points.
pub fn build_relation_point_row(rel_id: i64, member_coords: &[Vec<(f64, f64)>]) -> Option<GeomRow> {
    let coords: Vec<(f64, f64)> = member_coords.iter().flatten().copied().collect();
    if coords.is_empty() {
        return None;
    }
    let geom = project_line(&coords);
    let (x, y) = centroid_of_line(&geom)?;
    Some(GeomRow { osm_id: rel_id, geom_type: "Point", geom_ewkb: point_to_ewkb(x, y), length_m: None })
}
