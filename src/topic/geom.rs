//! Topic-independent geometry table rows: graph edges (`GeomRow`), whole-way linestrings
//! (`WayGeomRow`), and graph vertices (`NodeRow`). Same for every topic and every side object —
//! computed once per way/node, separate from `pipeline`'s per-topic, per-object tag/field rows.

use crate::osm::types::OsmWay;
use crate::output::{
    geometry::{haversine_length_m, point_to_ewkb, to_ewkb, wgs84_to_3857},
    rows::{GeomRow, NodeRow, WayGeomRow},
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

/// Build the whole-way linestring row for a way, emitted only with `--emit-way-geometries`.
pub fn build_way_geom_row(way: &OsmWay, geom: &geo::LineString<f64>, length_m: f64) -> WayGeomRow {
    WayGeomRow { osm_id: way.id, geom_ewkb: to_ewkb(geom), length_m }
}

/// Build a `nodes` table row for a graph vertex — always emitted (see `assign_node_ids`).
pub fn build_node_row(id: i64, osm_id: i64, lon: f64, lat: f64) -> NodeRow {
    let (x, y) = wgs84_to_3857(lon, lat);
    NodeRow { id, osm_id, geom_ewkb: point_to_ewkb(x, y) }
}
