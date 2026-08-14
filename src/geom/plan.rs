//! `GeometryPlan`: precomputed, once at startup from `&[TopicRunner]`, every geometry decision
//! that used to be a handful of separately-named `Vec<usize>`/`Vec<bool>` locals in `main.rs`. One
//! struct, built once, consumed by both `main.rs` (table/writer setup) and `geom::materialize`
//! (which shape to build) — so the "what does topic i want" question is answered in exactly one
//! place instead of being re-derived ad hoc at each call site.
//!
//! Simpler than it used to be because a topic now has *at most one* geometry shape per kind (see
//! `GeometryOutputSpec`'s own doc) — so "topics wanting any way shape" and "which shape each one
//! wants" are two small parallel arrays instead of three separate, possibly-overlapping topic-index
//! lists (`way_line_topics`/`way_polygon_topics`/`point_topics`) plus eligibility bools to
//! disambiguate a shared `point` table between node and way. Each kind also gets its own geometry
//! table now (`{table}_node_geom`/`{table}_way_geom`/`{table}_relation_geom`, `geom_type`
//! distinguishing Point/Line/Polygon within it — see `db::schema`), so there's no more cross-kind
//! table sharing to track here either.

use crate::osm::types::ElementKind;
use crate::topic::runner::TopicRunner;
use crate::topic::spec::GeometryShape;

pub struct GeometryPlan {
    /// Whether *any* topic wants the routing graph (`"graph": { "way": true }`) — the shared
    /// `edges`/`nodes` tables exist solely to back this; see `db::schema::EDGE_TABLE`'s doc.
    pub any_way_graph: bool,
    /// OR of every topic index bit wanting `"graph": { "node": true }` — a node classified by a
    /// topic whose bit is set here forces a graph cut point; classified only by a topic without the
    /// graph flag, it doesn't. See `GraphSpec`'s own doc.
    pub node_graph_mask: u32,

    /// Topic indices wanting a node point — always `Point` (the only shape `GeometryOutputSpec`
    /// allows for `node`), so no parallel shape array needed.
    pub node_geom_topics: Vec<usize>,

    /// Topic indices wanting any way geometry, and the shape each one wants (parallel array, same
    /// length/order) — a topic's `osm_id`-th shape row is built once by `geom::builders::way`
    /// (topic-independent) but only wanted way shapes get built at all, so `geom::materialize::way`
    /// still needs to know the *set* of wanted shapes across every topic in this list, not just one.
    pub way_geom_topics: Vec<usize>,
    pub way_shape: Vec<GeometryShape>,

    /// Topic indices wanting any relation geometry, and the shape each one wants (parallel array).
    pub relation_geom_topics: Vec<usize>,
    pub relation_shape: Vec<GeometryShape>,
    /// OR of every `relation_geom_topics` bit — `classify_rel_cb`'s cheap gate for whether a kept
    /// relation is even worth recording for relation-geometry resolution.
    pub relation_geom_mask: u32,
}

impl GeometryPlan {
    pub fn build(runners: &[TopicRunner]) -> Self {
        let geom_topics_and_shapes = |kind: ElementKind| -> (Vec<usize>, Vec<GeometryShape>) {
            (0..runners.len())
                .filter_map(|i| runners[i].geometry_output(kind).map(|shape| (i, shape)))
                .unzip()
        };

        let (node_geom_topics, _) = geom_topics_and_shapes(ElementKind::Node);
        let (way_geom_topics, way_shape) = geom_topics_and_shapes(ElementKind::Way);
        let (relation_geom_topics, relation_shape) = geom_topics_and_shapes(ElementKind::Relation);

        let node_graph_mask = (0..runners.len())
            .filter(|&i| runners[i].wants_node_graph())
            .fold(0u32, |m, i| m | (1 << i));
        let relation_geom_mask = relation_geom_topics.iter().fold(0u32, |m, &i| m | (1 << i));

        GeometryPlan {
            any_way_graph: runners.iter().any(|r| r.wants_way_graph()),
            node_graph_mask,
            node_geom_topics,
            way_geom_topics,
            way_shape,
            relation_geom_topics,
            relation_shape,
            relation_geom_mask,
        }
    }
}
