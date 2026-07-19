//! `GeometryPlan`: precomputed, once at startup from `&[TopicRunner]`, every geometry decision
//! that used to be a handful of separately-named `Vec<usize>`/`Vec<bool>` locals in `main.rs`
//! (`way_linestring_topics`, `way_polygon_topics`, `point_topics`, `way_point_eligible`, ...). One
//! struct, built once, consumed by both `main.rs` (table/writer setup) and `geom::materialize`
//! (which shape to build) — so the "what does topic i want" question is answered in exactly one
//! place instead of being re-derived ad hoc at each call site.

use crate::osm::types::ElementKind;
use crate::topic::runner::TopicRunner;
use crate::topic::spec::GeometryShape;

pub struct GeometryPlan {
    /// Whether *any* topic wants the routing graph (`"geometry": { "way": ["graph"] }`) — the
    /// shared `edges`/`nodes` tables exist solely to back this; see `db::schema::EDGE_TABLE`'s doc.
    pub any_way_graph: bool,
    /// Topic indices wanting a way's whole linestring.
    pub way_line_topics: Vec<usize>,
    /// Topic indices wanting a way's closed-ring polygon.
    pub way_polygon_topics: Vec<usize>,
    /// Topic indices wanting a `point` on *either* `node` or `way` — they share one
    /// `{table}_point` table, so this is the union; `way_point_eligible`/`node_point_eligible`
    /// (parallel to this list) say which kind(s) each entry actually applies to.
    pub point_topics: Vec<usize>,
    pub way_point_eligible: Vec<bool>,
    pub node_point_eligible: Vec<bool>,
    /// Topic indices wanting relation `line`/`point`/`polygon`, and the OR-ed bitmask of all three
    /// — `classify_rel_cb`'s cheap gate for whether a kept relation is even worth recording for
    /// relation-geometry resolution.
    pub relation_line_topics: Vec<usize>,
    pub relation_point_topics: Vec<usize>,
    pub relation_polygon_topics: Vec<usize>,
    pub relation_geom_mask: u32,
}

impl GeometryPlan {
    pub fn build(runners: &[TopicRunner]) -> Self {
        let indices_wanting = |kind: ElementKind, shape: GeometryShape| -> Vec<usize> {
            (0..runners.len()).filter(|&i| runners[i].wants(kind, shape)).collect()
        };

        let way_line_topics = indices_wanting(ElementKind::Way, GeometryShape::Line);
        let way_polygon_topics = indices_wanting(ElementKind::Way, GeometryShape::Polygon);
        let way_point: Vec<usize> = indices_wanting(ElementKind::Way, GeometryShape::Point);
        let node_point: Vec<usize> = indices_wanting(ElementKind::Node, GeometryShape::Point);
        let mut point_topics: Vec<usize> = way_point.iter().chain(&node_point).copied().collect();
        point_topics.sort_unstable();
        point_topics.dedup();
        let way_point_eligible = point_topics.iter().map(|i| way_point.contains(i)).collect();
        let node_point_eligible = point_topics.iter().map(|i| node_point.contains(i)).collect();

        let relation_line_topics = indices_wanting(ElementKind::Relation, GeometryShape::Line);
        let relation_point_topics = indices_wanting(ElementKind::Relation, GeometryShape::Point);
        let relation_polygon_topics = indices_wanting(ElementKind::Relation, GeometryShape::Polygon);
        let relation_geom_mask = relation_line_topics
            .iter()
            .chain(&relation_point_topics)
            .chain(&relation_polygon_topics)
            .fold(0u32, |m, &i| m | (1 << i));

        GeometryPlan {
            any_way_graph: runners.iter().any(|r| r.wants(ElementKind::Way, GeometryShape::Graph)),
            way_line_topics,
            way_polygon_topics,
            point_topics,
            way_point_eligible,
            node_point_eligible,
            relation_line_topics,
            relation_point_topics,
            relation_polygon_topics,
            relation_geom_mask,
        }
    }
}
