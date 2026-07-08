#[path = "../../../src/classify/mod.rs"]
pub mod classify;
#[path = "../../../src/engine/mod.rs"]
pub mod engine;
#[path = "../../../src/lint/mod.rs"]
pub mod lint;
#[path = "../../../src/osm/types.rs"]
pub mod osm_types;
#[path = "../../../src/output/types.rs"]
pub mod output_types;
#[path = "../../../src/output/rows.rs"]
pub mod output_rows;
#[path = "../../../src/output/geometry.rs"]
pub mod output_geometry;
#[path = "../../../src/transform/mod.rs"]
pub mod transform;
#[path = "../../../src/value_sets.rs"]
pub mod value_sets;
#[path = "../../../src/paths.rs"]
pub mod paths;
#[path = "../../../src/profile.rs"]
pub mod profile;
#[path = "../../../src/traffic.rs"]
pub mod traffic;

pub mod osm {
    pub use crate::osm_types as types;
}
pub mod output {
    pub use crate::output_geometry as geometry;
    pub use crate::output_rows as rows;
    pub use crate::output_types as types;
}
