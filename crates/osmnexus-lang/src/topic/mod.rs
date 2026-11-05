//! The compile side of a `topics/<name>/` directory: the `topic.json`/`transforms.json` schema
//! (`spec`), reading a topic's categories/macros/sanitizers/transforms out of `ConfigSources`
//! (`load`, `sources`), the loaded topic itself (`model::Topic`), and its inferred output schema
//! (`schema`). Running a topic over elements is the `osmnexus` crate's `topic::pipeline`.

pub mod load;
pub mod model;
pub mod schema;
pub mod sources;
pub mod spec;

pub use model::Topic;
