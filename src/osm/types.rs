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

/// Way tags. FxHashMap (not the default SipHash) because categorize + extract do millions of
/// `.get()` lookups and per-way/per-side-object clones of these maps — the hot path of Pass C.
pub type RawTags = rustc_hash::FxHashMap<String, String>;

/// A way resolved to geometry. Tags/meta are *not* carried here — classification is tag-only and
/// happens in Pass A from `WayData`; this type exists only for the geometry pass, which needs the
/// projected coordinates and graph cut-points.
pub struct OsmWay {
    pub id: i64,
    /// WGS84 coordinates in (lon, lat) order.
    pub coords: Vec<(f64, f64)>,
    /// Graph-vertex cut points as `(index into coords, node_id)`, ascending by index: the way's
    /// start and end nodes (always), plus every interior node shared with another way (occurrence
    /// count > 1). Segments = consecutive cut-point pairs; intermediate geometry nodes are not
    /// retained. Lets a way be split into graph edges purely by index, no geometric matching.
    pub cut_points: Vec<(u32, i64)>,
}

/// A filter-passing way's tags + node refs + metadata, produced by the reader's single way-region
/// decode (Pass A). Tag-only classification consumes this; the geometry pass keeps only `node_refs`.
pub struct WayData {
    pub id: i64,
    pub tags: RawTags,
    pub node_refs: Vec<i64>,
    pub meta: WayMeta,
}

pub struct WayMeta {
    /// Unix timestamp (seconds since epoch), if available.
    pub timestamp: Option<i64>,
    pub user: Option<String>,
    pub changeset: Option<i64>,
}

/// A relation's tags + metadata + member **way** ids, produced by the relations pass. Node/relation
/// members are ignored — only way members are pulled into the graph. Classification is tag-only;
/// the member ids feed the reader's relation-member keep set (member ways are kept even if their own
/// tags match nothing) and the `relation_members` link output.
pub struct RelData {
    pub id: i64,
    pub tags: RawTags,
    pub member_ways: Vec<i64>,
    pub meta: WayMeta,
}

/// A node's tags + metadata, produced by the nodes pass for nodes that are members of a kept way.
/// Classification is tag-only; a selected node becomes a forced graph-vertex cut point.
pub struct NodeData {
    pub id: i64,
    pub tags: RawTags,
    pub meta: WayMeta,
}

