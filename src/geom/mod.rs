//! All geometry logic in one place, separate from the topic/config/tag-classification engine
//! (`categorize`/`lang`/`topic`) and the DB/CSV output plumbing (`db`/`output`), both of which only
//! ever consume this module's row types and builder functions:
//! - `primitives`: WGS84↔Web Mercator projection, length/centroid, EWKB encode/decode — the raw
//!   geometric math, no OSM/topic concepts.
//! - `rows`: the geometry-specific output row types (`EdgeRow`/`GeomRow`/`NodeRow`) and their CSV
//!   column layouts. `TopicRow`/`MemberRow` (not geometry-specific) stay in `output::rows`.
//! - `builders`: turns a resolved `OsmWay` (or a relation's member-way coordinates) into the row
//!   types above — one function per shape (`point`/`line`/`graph`/`polygon`) per kind.
//! - `relation`: ring assembly for `Polygon`/multipolygon from a relation's member ways' resolved
//!   coordinates. Resolving those coordinates isn't here — it rides along in `osm::reader`'s own
//!   Pass A/B decode as a side channel (`Callbacks::extra_way_ids`/`build_extra_geom`), so relation
//!   geometry costs no second PBF scan.
//! - `plan`: `GeometryPlan`, precomputed once from `&[TopicRunner]` — which topics want which
//!   shape, in one place instead of scattered `Vec<usize>` locals.
//! - `materialize`: given a resolved element + `GeometryPlan`, decide which shapes are wanted and
//!   build all their rows in one call — the actual "materialize" half of the select/materialize
//!   split; `main.rs`'s job shrinks to routing whatever rows come back to writer channels.

pub mod builders;
pub mod materialize;
pub mod plan;
pub mod primitives;
pub mod relation;
pub mod rows;
