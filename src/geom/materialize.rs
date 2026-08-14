//! The "materialize" half of the select/materialize split: given a resolved element (a way's
//! `OsmWay`, a node's coordinate, or a relation's member-way coordinates) plus a `GeometryPlan`,
//! decide which shapes are actually wanted and build every row for them — one call site per
//! element kind, replacing what used to be a handful of `if !topics.is_empty() { ... }` blocks
//! inlined directly in `main.rs`'s reader callbacks. Pure computation, no I/O: callers (`main.rs`)
//! still own routing the returned rows to writer channels and reporting counts.

use rustc_hash::{FxHashMap, FxHashSet};

use crate::geom::builders::{
    build_edges, build_node_point_row, build_node_row, build_relation_line_row, build_relation_point_row,
    build_relation_polygon_row, build_way, build_way_point_row, build_way_polygon_row,
};
use crate::geom::plan::GeometryPlan;
use crate::geom::primitives::{haversine_length_m, project_line};
use crate::geom::relation::assemble_rings;
use crate::geom::rows::{EdgeRow, GeomRow, NodeRow};
use crate::osm::reader::{assign_node_ids, SelectionContext};
use crate::osm::types::{MemberRole, OsmWay};
use crate::topic::spec::GeometryShape;

/// Every shape a way can produce, already gated by `plan` — `None` for any shape no topic wants
/// (or, for `point`, that a way's coordinates happened to collapse to a degenerate centroid). Up
/// to three of these can be non-`None` at once (different topics can want different shapes for the
/// same way — see `GeometryPlan::way_shape`'s own doc), even though each individual topic only
/// ever gets routed the one it declared.
pub struct WayGeometry {
    pub edges: Option<Vec<EdgeRow>>,
    pub line: Option<GeomRow>,
    pub point: Option<GeomRow>,
    pub polygon: Option<GeomRow>,
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

    let want_line = plan.way_shape.contains(&GeometryShape::Line);
    let want_point = plan.way_shape.contains(&GeometryShape::Point);
    let want_polygon = plan.way_shape.contains(&GeometryShape::Polygon);
    let geom = (want_line || want_point).then(|| project_line(&way.coords));

    let line = geom.as_ref().filter(|_| want_line).map(|g| {
        let length_m = haversine_length_m(&way.coords);
        build_way(way, g, length_m)
    });
    let point = geom.as_ref().filter(|_| want_point).and_then(|g| build_way_point_row(way, g));

    let polygon = want_polygon.then(|| build_way_polygon_row(way));

    WayGeometry { edges, line, point, polygon }
}

/// A node's own point row, gated on whether any topic wants `"geometry_output": { "node": "point" }`
/// — trivial, but kept alongside `way`/`relations` for a uniform "ask the plan, get rows back" shape
/// at every call site. Always `Point` (the only shape a node can declare — see
/// `GeometryOutputSpec::validate`), so no shape check needed like `way`'s.
pub fn node_point(osm_id: i64, lon: f64, lat: f64, plan: &GeometryPlan) -> Option<GeomRow> {
    (!plan.node_geom_topics.is_empty()).then(|| build_node_point_row(osm_id, lon, lat))
}

/// One relation's requested geometry — up to three of these can be non-`None` at once (see
/// `WayGeometry`'s own doc, same reasoning). The caller routes each wanting topic its one declared
/// shape.
pub struct RelationGeometry {
    pub line: Option<GeomRow>,
    pub point: Option<GeomRow>,
    pub polygon: Option<GeomRow>,
}

/// Build one relation's line/point/polygon from its member ways' already-resolved coordinates
/// (see `geom::relation`'s own doc on how those got resolved with no second PBF scan) — `members`
/// is `(way_id, role)` pairs, `way_coords` the resolved-coordinate lookup. `polygon` is only
/// attempted when some topic wants it (ring assembly is the one non-trivial cost here).
pub fn relation(
    members: &[(i64, MemberRole)],
    way_coords: &FxHashMap<i64, Vec<(f64, f64)>>,
    rel_id: i64,
    plan: &GeometryPlan,
) -> RelationGeometry {
    let member_coords: Vec<Vec<(f64, f64)>> =
        members.iter().filter_map(|&(w, _)| way_coords.get(&w).cloned()).collect();

    let want_line = plan.relation_shape.contains(&GeometryShape::Line);
    let want_point = plan.relation_shape.contains(&GeometryShape::Point);
    let want_polygon = plan.relation_shape.contains(&GeometryShape::Polygon);

    let line = want_line.then(|| build_relation_line_row(rel_id, &member_coords)).flatten();
    let point = want_point.then(|| build_relation_point_row(rel_id, &member_coords)).flatten();
    let polygon = want_polygon
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

/// Every kept, geometry-wanting relation's rows, one `Vec` per wanting topic (parallel to
/// `plan.relation_geom_topics`) — the batch-oriented sibling of `way`/`node_point`, since relation
/// geometry is resolved after the whole streaming pass finishes (see `geom::relation`'s own doc)
/// rather than one element at a time as the file streams.
pub struct RelationGeomBatch {
    pub rows: Vec<Vec<GeomRow>>,
}

pub fn relations(
    requests: &[(i64, Vec<(i64, MemberRole)>, u32)],
    way_coords: &FxHashMap<i64, Vec<(f64, f64)>>,
    plan: &GeometryPlan,
) -> RelationGeomBatch {
    let mut rows: Vec<Vec<GeomRow>> = vec![Vec::new(); plan.relation_geom_topics.len()];

    for (rel_id, members, mask) in requests {
        let g = relation(members, way_coords, *rel_id, plan);
        for (i, &topic_idx) in plan.relation_geom_topics.iter().enumerate() {
            if mask & (1 << topic_idx) == 0 {
                continue;
            }
            let row = match plan.relation_shape[i] {
                GeometryShape::Line => &g.line,
                GeometryShape::Point => &g.point,
                GeometryShape::Polygon => &g.polygon,
            };
            if let Some(row) = row {
                rows[i].push(row.clone());
            }
        }
    }

    RelationGeomBatch { rows }
}

/// Every row `run` produced, ready for `main.rs` to route to writer channels — the top-level
/// entry point of the "materialize" phase: everything below this is `run`'s own implementation.
/// Way shapes are *not* included here — `run` routes each way's `WayGeometry` to `route_way`
/// itself, one way at a time as its own `par_iter` resolves it, instead of collecting every way's
/// (already-serialized) shapes into one big `Vec` before any of it can be written out (that used to
/// mean the whole run's way-output rows were resident in memory simultaneously, on top of
/// `SelectionContext::node_coords` and the `resolved` map below — see this module's own history).
pub struct MaterializedGeometry {
    /// `osm node id -> internal graph-vertex id` — empty unless `plan.any_way_graph`.
    pub node_ids: FxHashMap<i64, i64>,
    pub node_rows: Vec<NodeRow>,
    pub relations: RelationGeomBatch,
}

/// Run the whole materialize phase over a finished `SelectionContext`: resolve every referenced
/// way's coordinates once (shared between way-shape building and relation-geometry assembly, so a
/// way that's both independently kept *and* a relation member is only resolved once), assign graph
/// vertex ids if needed, and build every shape every topic asked for. Each mask-!=0 way's shapes are
/// handed to `route_way(way_id, mask, shapes)` as soon as they're built, from whichever `rayon`
/// worker resolved that way — `route_way` must be safe to call concurrently (the caller's
/// `TableWriters::route_way` already is, called the same way from the select phase's callbacks).
pub fn run<F>(ctx: &SelectionContext, plan: &GeometryPlan, route_way: F) -> MaterializedGeometry
where
    F: Fn(i64, u32, WayGeometry) + Sync,
{
    let (node_ids, node_rows) = if plan.any_way_graph {
        let endpoints: FxHashSet<i64> = ctx.way_refs.endpoints();
        let (ids, rows) = assign_node_ids(&ctx.node_coords, &endpoints, &ctx.selected);
        let node_rows = rows.into_iter().map(|(id, osm_id, lon, lat)| build_node_row(id, osm_id, lon, lat)).collect();
        (ids, node_rows)
    } else {
        (FxHashMap::default(), Vec::new())
    };

    // Resolve + route every kept way's own shapes, in `ctx.kept_way_order` — the same order the
    // select phase already routed that way's tag row in (see `WayRefsStore::par_route_ordered`'s own
    // doc) — resolved once, routed, dropped; no cache of every kept way's geometry stays resident.
    ctx.way_refs.par_route_ordered(&ctx.kept_way_order, &ctx.node_coords, &ctx.selected, |id, mask, w| {
        route_way(id, mask, way(&w, &node_ids, plan));
    });

    // Relation-geometry assembly needs its own coordinate copy — skip entirely when no topic wants
    // relation line/point/polygon (`relations()` would build nothing from it either way), and even
    // then resolve only the (typically small — see `par_route_ordered`'s own doc) subset of ways
    // that are actual relation members, redoing `resolve_geometry` for any that were already
    // resolved above rather than caching every kept way's geometry just to avoid that overlap's
    // redundant work.
    let want_relation_geom = !plan.relation_geom_topics.is_empty();
    let way_coords: FxHashMap<i64, Vec<(f64, f64)>> = if want_relation_geom && !ctx.rel_members.is_empty() {
        let member_way_ids = ctx.rel_members.member_way_ids();
        member_way_ids
            .iter()
            .filter_map(|&id| ctx.way_refs.resolve_one(id, &ctx.node_coords, &ctx.selected).map(|w| (id, w.coords)))
            .collect()
    } else {
        FxHashMap::default()
    };
    let requests: Vec<(i64, Vec<(i64, MemberRole)>, u32)> = ctx.rel_members.requests_ordered(&ctx.kept_relation_order);
    let relations_batch = relations(&requests, &way_coords, plan);

    MaterializedGeometry { node_ids, node_rows, relations: relations_batch }
}
