pub use crate::lang::tags::{ElementKind, RawTags};

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

    /// Compact byte tag for `EncodedMembers`' on-disk/in-arena representation — not tied to any
    /// wire format, just stable within this process.
    pub fn as_u8(self) -> u8 {
        match self {
            MemberRole::Outer => 0,
            MemberRole::Inner => 1,
            MemberRole::Unknown => 2,
        }
    }

    /// Inverse of `as_u8`. Any byte other than 0/1 reads as `Unknown` — same "unrecognized role"
    /// fallback `from_str` already uses, so a hypothetical future role byte still round-trips.
    pub fn from_u8(b: u8) -> Self {
        match b {
            0 => MemberRole::Outer,
            1 => MemberRole::Inner,
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

