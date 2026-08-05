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
/// Values are `Cow<'a, str>`, not `String`: a way/node/relation's tags start out borrowed straight
/// from the pbf block's string table (no per-element allocation for elements no topic keeps — see
/// `osm::reader::resolve::way_data`), and only become `Cow::Owned` where a transform step
/// (`categorize::transform::InputTransform`) computes and inserts a new value. A bonus: cloning a
/// still-all-borrowed map (`topic::pipeline::build_topic_rows`'s per-topic `Cow<RawTags>` upgrade)
/// copies pointer+len per entry instead of deep-copying every string.
pub type RawTags<'a> = rustc_hash::FxHashMap<std::borrow::Cow<'a, str>, std::borrow::Cow<'a, str>>;

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

/// A filter-passing way's tags + metadata, produced by the reader's single way-region decode
/// (Pass A) — purely what tag-only `classify` needs. Node refs aren't here: see `way_data`'s own
/// doc for why they're read directly off `osmpbf::Way::raw_refs()` at the Pass A call site instead.
pub struct WayData<'a> {
    pub id: i64,
    pub tags: RawTags<'a>,
    pub meta: WayMeta,
}

pub struct WayMeta {
    /// Unix timestamp (seconds since epoch), if available.
    pub timestamp: Option<i64>,
    pub user: Option<String>,
    pub changeset: Option<i64>,
}

/// A relation member way's role in the relation's geometry — `outer`/`inner` per the multipolygon
/// convention (see `geom::relation`'s ring-assembly doc); any other role string (or a
/// role-less member, e.g. a plain route relation) is `Unknown` — geometrically just another
/// segment to chain, no hole/outer distinction (relevant only for `Polygon` assembly).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemberRole {
    Outer,
    Inner,
    Unknown,
}

impl MemberRole {
    pub fn from_str(s: &str) -> Self {
        match s {
            "outer" => MemberRole::Outer,
            "inner" => MemberRole::Inner,
            _ => MemberRole::Unknown,
        }
    }
}

/// A relation's tags + metadata + member **way** ids (with role), produced by the relations pass.
/// Node/relation members are ignored — only way members are pulled into the graph. Classification
/// is tag-only; the member ids feed the `relation_members` link output and (independently, see
/// `main.rs`) relation geometry construction.
pub struct RelData<'a> {
    pub id: i64,
    pub tags: RawTags<'a>,
    pub member_ways: Vec<(i64, MemberRole)>,
    pub meta: WayMeta,
}

/// A node's tags + metadata + coords, produced by the nodes pass for nodes that are members of a
/// kept way. Classification is tag-only; a selected node becomes a forced graph-vertex cut point.
/// `lon`/`lat` (WGS84) ride along so `--emit-node-geometries` can build a point row without a
/// second lookup.
pub struct NodeData<'a> {
    pub id: i64,
    pub tags: RawTags<'a>,
    pub meta: WayMeta,
    pub lon: f64,
    pub lat: f64,
}

