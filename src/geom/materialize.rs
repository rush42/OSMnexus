//! The "materialize" half of the select/materialize split: given a resolved element (a way's
//! `OsmWay`, a node's coordinate, or a relation's member-way coordinates) plus a `GeometryPlan`,
//! decide which shapes are actually wanted and build every row for them — one call site per
//! element kind, replacing what used to be a handful of `if !topics.is_empty() { ... }` blocks
//! inlined directly in `main.rs`'s reader callbacks. Pure computation, no I/O: callers (`main.rs`)
//! still own routing the returned rows to writer channels and reporting counts.

use rayon::prelude::*;
use rustc_hash::{FxHashMap, FxHashSet};

use crate::geom::builders::{
    build_edges, build_node_point_row, build_node_row, build_relation_line_row, build_relation_point_row,
    build_relation_polygon_row, build_way, build_way_point_row, build_way_polygon_row,
};
use crate::geom::plan::GeometryPlan;
use crate::geom::primitives::{haversine_length_m, project_line};
use crate::geom::relation::assemble_rings;
use crate::geom::rows::{EdgeRow, NodeRow, PointRow, PolygonRow, WayRow};
use crate::osm::reader::{assign_node_ids, resolve_geometry, SelectionContext};
use crate::osm::types::{MemberRole, OsmWay};

/// Every shape a way can produce, already gated by `plan` — `None` for any shape no topic wants
/// (or, for `point`, that a way's coordinates happened to collapse to a degenerate centroid).
pub struct WayGeometry {
    pub edges: Option<Vec<EdgeRow>>,
    pub line: Option<WayRow>,
    pub point: Option<PointRow>,
    pub polygon: Option<PolygonRow>,
}

/// Build every shape a resolved way needs, per `plan`. `node_ids` is only consulted when
/// `plan.any_way_graph` (edges need internal vertex ids); the line/point projection is computed at
/// most once and shared between the `line` and `point` shapes when both are wanted.
pub fn way(way: &OsmWay, node_ids: &FxHashMap<i64, i64>, plan: &GeometryPlan) -> WayGeometry {
    let edges = plan.any_way_graph.then(|| {
        let geom = project_line(&way.coords);
        let length_m = haversine_length_m(&way.coords);
        build_edges(way, &geom, length_m, node_ids)
    });

    let want_line = !plan.way_line_topics.is_empty();
    let want_point = !plan.point_topics.is_empty();
    let geom = (want_line || want_point).then(|| project_line(&way.coords));

    let line = geom.as_ref().filter(|_| want_line).map(|g| {
        let length_m = haversine_length_m(&way.coords);
        build_way(way, g, length_m)
    });
    let point = geom.as_ref().filter(|_| want_point).and_then(|g| build_way_point_row(way, g));

    let polygon = (!plan.way_polygon_topics.is_empty()).then(|| build_way_polygon_row(way));

    WayGeometry { edges, line, point, polygon }
}

/// A node's own point row, gated on whether any topic wants `"geometry": { "node": ["point"] }` —
/// trivial, but kept alongside `way`/`relations` for a uniform "ask the plan, get rows back" shape
/// at every call site.
pub fn node_point(osm_id: i64, lon: f64, lat: f64, plan: &GeometryPlan) -> Option<PointRow> {
    (!plan.point_topics.is_empty()).then(|| build_node_point_row(osm_id, lon, lat))
}

/// One relation's requested geometry, already fanned out per wanting topic (`line_topics[i]`'s
/// mask bit decides whether `line`/`point`/`polygon` apply to that topic) — the caller just needs
/// to route `Some` rows to their topic's channel.
pub struct RelationGeometry {
    pub line: Option<WayRow>,
    pub point: Option<PointRow>,
    pub polygon: Option<PolygonRow>,
}

/// Build one relation's line/point/polygon from its member ways' already-resolved coordinates
/// (see `geom::relation`'s own doc on how those got resolved with no second PBF scan) — `members`
/// is `(way_id, role)` pairs, `way_coords` the resolved-coordinate lookup. `polygon` is only
/// attempted when `plan.relation_polygon_topics` is non-empty (ring assembly is the one
/// non-trivial cost here).
pub fn relation(
    members: &[(i64, MemberRole)],
    way_coords: &FxHashMap<i64, Vec<(f64, f64)>>,
    rel_id: i64,
    plan: &GeometryPlan,
) -> RelationGeometry {
    let member_coords: Vec<Vec<(f64, f64)>> =
        members.iter().filter_map(|&(w, _)| way_coords.get(&w).cloned()).collect();

    let line = (!plan.relation_line_topics.is_empty())
        .then(|| build_relation_line_row(rel_id, &member_coords))
        .flatten();
    let point = (!plan.relation_point_topics.is_empty())
        .then(|| build_relation_point_row(rel_id, &member_coords))
        .flatten();
    let polygon = (!plan.relation_polygon_topics.is_empty())
        .then(|| {
            let outer: Vec<_> = members
                .iter()
                .filter(|&&(_, role)| role != MemberRole::Inner)
                .filter_map(|&(w, _)| way_coords.get(&w).cloned())
                .collect();
            let inner: Vec<_> = members
                .iter()
                .filter(|&&(_, role)| role == MemberRole::Inner)
                .filter_map(|&(w, _)| way_coords.get(&w).cloned())
                .collect();
            build_relation_polygon_row(rel_id, &assemble_rings(outer), &assemble_rings(inner))
        })
        .flatten();

    RelationGeometry { line, point, polygon }
}

/// Every kept, geometry-wanting relation's rows, already fanned out into one `Vec` per wanting
/// topic (parallel to `plan.relation_{line,point,polygon}_topics`) — the batch-oriented sibling of
/// `way`/`node_point`, since relation geometry is resolved after the whole streaming pass finishes
/// (see `geom::relation`'s own doc) rather than one element at a time as the file streams.
pub struct RelationGeomBatch {
    pub line_rows: Vec<Vec<WayRow>>,
    pub point_rows: Vec<Vec<PointRow>>,
    pub polygon_rows: Vec<Vec<PolygonRow>>,
}

pub fn relations(
    requests: &[(i64, Vec<(i64, MemberRole)>, u32)],
    way_coords: &FxHashMap<i64, Vec<(f64, f64)>>,
    plan: &GeometryPlan,
) -> RelationGeomBatch {
    let mut line_rows: Vec<Vec<WayRow>> = vec![Vec::new(); plan.relation_line_topics.len()];
    let mut point_rows: Vec<Vec<PointRow>> = vec![Vec::new(); plan.relation_point_topics.len()];
    let mut polygon_rows: Vec<Vec<PolygonRow>> = vec![Vec::new(); plan.relation_polygon_topics.len()];

    for (rel_id, members, mask) in requests {
        let g = relation(members, way_coords, *rel_id, plan);
        if let Some(row) = g.line {
            for (i, &topic_idx) in plan.relation_line_topics.iter().enumerate() {
                if mask & (1 << topic_idx) != 0 {
                    line_rows[i].push(row.clone());
                }
            }
        }
        if let Some(row) = g.point {
            for (i, &topic_idx) in plan.relation_point_topics.iter().enumerate() {
                if mask & (1 << topic_idx) != 0 {
                    point_rows[i].push(row.clone());
                }
            }
        }
        if let Some(row) = g.polygon {
            for (i, &topic_idx) in plan.relation_polygon_topics.iter().enumerate() {
                if mask & (1 << topic_idx) != 0 {
                    polygon_rows[i].push(row.clone());
                }
            }
        }
    }

    RelationGeomBatch { line_rows, point_rows, polygon_rows }
}

/// Every row `run` produced, ready for `main.rs` to route to writer channels — the top-level
/// entry point of the "materialize" phase: everything below this is `run`'s own implementation.
pub struct MaterializedGeometry {
    /// `osm node id -> internal graph-vertex id` — empty unless `plan.any_way_graph`.
    pub node_ids: FxHashMap<i64, i64>,
    pub node_rows: Vec<NodeRow>,
    /// One entry per way with a non-zero keep mask (relation-member-only ways are never
    /// materialized for their own shapes — see `SelectionContext::way_refs`'s own doc) —
    /// `(way_id, mask, shapes)`.
    pub ways: Vec<(i64, u32, WayGeometry)>,
    pub relations: RelationGeomBatch,
}

/// Run the whole materialize phase over a finished `SelectionContext`: resolve every referenced
/// way's coordinates once (shared between way-shape building and relation-geometry assembly, so a
/// way that's both independently kept *and* a relation member is only resolved once), assign graph
/// vertex ids if needed, and build every shape every topic asked for.
pub fn run(ctx: &SelectionContext, plan: &GeometryPlan) -> MaterializedGeometry {
    // Resolve every referenced way once — mask-!=0 ways (for their own shapes) and mask-0
    // relation-member-only ways (for relation-geometry assembly) alike. `resolve_geometry`
    // already applies the "too short to be a line" (<2 resolvable points) rule uniformly.
    let resolved: FxHashMap<i64, OsmWay> = ctx
        .way_refs
        .par_iter()
        .filter_map(|(&id, (refs, _))| resolve_geometry(id, refs, &ctx.node_coords, &ctx.selected).map(|w| (id, w)))
        .collect();

    let (node_ids, node_rows) = if plan.any_way_graph {
        let endpoints: FxHashSet<i64> = ctx
            .way_refs
            .iter()
            .filter(|(_, (_, mask))| *mask != 0)
            .filter_map(|(_, (refs, _))| Some([*refs.first()?, *refs.last()?]))
            .flatten()
            .collect();
        let (ids, rows) = assign_node_ids(&ctx.node_coords, &endpoints, &ctx.selected);
        let node_rows = rows.into_iter().map(|(id, osm_id, lon, lat)| build_node_row(id, osm_id, lon as f64, lat as f64)).collect();
        (ids, node_rows)
    } else {
        (FxHashMap::default(), Vec::new())
    };

    let ways: Vec<(i64, u32, WayGeometry)> = ctx
        .way_refs
        .par_iter()
        .filter(|(_, (_, mask))| *mask != 0)
        .filter_map(|(&id, (_, mask))| resolved.get(&id).map(|w| (id, *mask, way(w, &node_ids, plan))))
        .collect();

    let way_coords: FxHashMap<i64, Vec<(f64, f64)>> =
        resolved.iter().map(|(&id, w)| (id, w.coords.clone())).collect();
    let requests: Vec<(i64, Vec<(i64, MemberRole)>, u32)> =
        ctx.rel_members.iter().map(|(&id, (members, mask))| (id, members.clone(), *mask)).collect();
    let relations_batch = relations(&requests, &way_coords, plan);

    MaterializedGeometry { node_ids, node_rows, ways, relations: relations_batch }
}
