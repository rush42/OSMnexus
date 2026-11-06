//! Tag-level types the language evaluates against: an element's tag map and which OSM primitive
//! it is. They live in `lang` (not `osm::types`, which re-exports them) because classification is
//! tag-only and the core must not depend on the OSM reader (see `boundaries_plan.md`, step 4).
//! `RawTags` keeps borrowing from the PBF block (`Cow<'a, str>`); don't make it owned.

/// Way tags. FxHashMap (not the default SipHash) because categorize + extract do millions of
/// `.get()` lookups and per-way/per-side-object clones of these maps — the hot path of Pass C.
/// Values are `Cow<'a, str>`, not `String`: a way/node/relation's tags start out borrowed straight
/// from the pbf block's string table (no per-element allocation for elements no topic keeps — see
/// `osm::reader::resolve::way_data`), and only become `Cow::Owned` where a transform step
/// (`categorize::transform::InputTransform`) computes and inserts a new value. A bonus: cloning a
/// still-all-borrowed map (`topic::pipeline::build_topic_rows`'s per-topic `Cow<RawTags>` upgrade)
/// copies pointer+len per entry instead of deep-copying every string.
pub type RawTags<'a> = rustc_hash::FxHashMap<std::borrow::Cow<'a, str>, std::borrow::Cow<'a, str>>;

/// Which OSM primitive an element (and a topic's category set) is. A topic organizes its categories
/// into per-kind subfolders (`topics/<t>/{node,way,relation}/`); each pass classifies one kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ElementKind {
    Node,
    Way,
    Relation,
}

impl ElementKind {
    pub const ALL: [ElementKind; 3] = [ElementKind::Node, ElementKind::Way, ElementKind::Relation];

    /// The `osm_type` column value: `N` / `W` / `R`.
    pub fn osm_type(self) -> &'static str {
        match self {
            ElementKind::Node => "N",
            ElementKind::Way => "W",
            ElementKind::Relation => "R",
        }
    }

    /// The `id` string prefix (`node` / `way` / `relation`), e.g. `way/123`.
    pub fn id_prefix(self) -> &'static str {
        match self {
            ElementKind::Node => "node",
            ElementKind::Way => "way",
            ElementKind::Relation => "relation",
        }
    }

    /// The category subfolder name under a topic dir.
    pub fn subdir(self) -> &'static str {
        self.id_prefix()
    }
}
