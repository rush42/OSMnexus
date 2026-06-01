pub mod construction_prefix;
pub mod cycleway_both;
pub mod lifecycle;
pub mod opposite;
pub mod side_split;

use crate::osm::types::RawTags;

/// A no-arg tag transform, referenced by name from a topic's `transforms` list.
/// These rewrite the tag map in place before categorization (e.g. normalizing
/// `highway=construction`, the `cycleway=opposite_*` schema, etc.).
#[derive(Debug, Clone, Copy)]
pub enum TagTransform {
    Lifecycle,
    CyclewayOpposite,
    ConstructionPrefix,
    CyclewayBoth,
}

impl TagTransform {
    /// Resolve a transform name from `topic.json` to its implementation.
    pub fn from_name(name: &str) -> anyhow::Result<Self> {
        Ok(match name {
            "lifecycle"           => TagTransform::Lifecycle,
            "cycleway_opposite"   => TagTransform::CyclewayOpposite,
            "construction_prefix" => TagTransform::ConstructionPrefix,
            "cycleway_both"       => TagTransform::CyclewayBoth,
            other => anyhow::bail!("unknown tag transform '{other}'"),
        })
    }

    pub fn apply(&self, tags: &mut RawTags) {
        match self {
            TagTransform::Lifecycle          => { lifecycle::transform_lifecycle_tags(tags); }
            TagTransform::CyclewayOpposite   => opposite::transform_cycleway_opposite_schema(tags),
            TagTransform::ConstructionPrefix => construction_prefix::transform_construction_prefix(tags),
            TagTransform::CyclewayBoth       => cycleway_both::transform_cycleway_both_postfix(tags),
        }
    }
}
