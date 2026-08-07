//! Non-geometry output row types (tag rows + relation-member links) and their CSV serialization,
//! plus the shared `CsvRow` trait/`write_csv_row` helper both this module and `geom::rows` use.
//! Geometry row types (`EdgeRow`/`WayRow`/`NodeRow`/`PointRow`/`PolygonRow`) live in
//! `geom::rows` instead — see `geom`'s own module doc for why geometry is split out.

/// Column lists shared by the COPY statement and the CSV header line (no spaces → valid as both).
/// The field order here **must** match each row type's `csv_fields` implementation below.
pub const TAG_COLUMNS: &str = "osm_id,osm_type,id,category,produced,annotations,meta";
pub const MEMBER_COLUMNS: &str = "relation_osm_id,way_osm_id";

/// A row that can be serialized into an ordered list of CSV fields. Implemented by every output
/// row type (here and in `geom::rows`) so the writers (`output::writers`) can be generic over the
/// row type.
pub trait CsvRow {
    /// The CSV fields in `*_COLUMNS` order, consuming `self` — lets `TopicRow` move its
    /// already-JSON-encoded `produced`/`annotations`/`meta` `String`s straight out instead of
    /// cloning them (the writer never needs the row again after this call; every implementor's
    /// other fields are equally happy moved as borrowed).
    fn csv_fields(self) -> anyhow::Result<Vec<String>>;
}

/// A single tag row produced by the topic engine — one per (way, side, prefix), independent of
/// how the geometry is later cut. Geometry lives in the paired geom table (see `EdgeRow`), joined
/// on `osm_id` at tile-materialization time.
pub struct TopicRow {
    pub osm_id: i64,
    pub osm_type: &'static str,
    pub id: String,
    /// The matched category's id — a dedicated column rather than a `produced` key, since every
    /// row has at most one and it's not itself a `Producer`-evaluated output. `None` for an
    /// `accept_all` kind (see `TopicSpec::accept_all`), which never matches a category at all;
    /// serializes as an empty CSV field, which Postgres `COPY ... FORMAT CSV` reads back as NULL.
    pub category: Option<String>,
    /// Every non-underscore-prefixed output (the former separate `osm`/`sanitized`/`derived`
    /// columns — all three were always the same `Producer`-evaluation mechanism, just different
    /// JSON shorthands for declaring one entry in one `producers` map; see `TopicSpec::producers`).
    /// Pre-serialized to its final JSON text by `topic::pipeline::build_topic_rows`, which runs on
    /// the rayon classify workers (up to 8-way parallel here) — not left as a `Map` for the writer
    /// task to serialize later, which would cap JSON encoding at `--db-writers`-way parallelism
    /// (4 by default) regardless of how many workers did the classifying. See `csv_fields` below,
    /// now just a move of these three strings.
    pub produced: String,
    /// Engine-attached bookkeeping about `produced`, not itself a topic-authored output: side-split
    /// context (`_side`/`_prefix`/`_infix`, stamped by `topic::pipeline::build_topic_rows`/each `Clone`)
    /// and each output's companion `annotate` provenance (`<output>_source`/`<output>_confidence`,
    /// from `Produced::annotate`) — see `topic::pipeline::eval_fields`. Pre-serialized, same reason
    /// as `produced`.
    pub annotations: String,
    pub meta: String,
}

impl CsvRow for TopicRow {
    /// CSV field order matches `TAG_COLUMNS`. `produced`/`annotations`/`meta` are already JSON
    /// text by construction (see their own docs) — moved straight out, no clone or
    /// `serde_json::to_string` left to do here.
    fn csv_fields(self) -> anyhow::Result<Vec<String>> {
        Ok(vec![
            self.osm_id.to_string(),
            self.osm_type.to_owned(),
            self.id,
            self.category.unwrap_or_default(),
            self.produced,
            self.annotations,
            self.meta,
        ])
    }
}

/// A relation → member-way link row, emitted for every way member of a kept relation. Lets a
/// relation's tag row be joined downstream to the geometries of its member ways (relations have no
/// materialized geometry of their own).
pub struct MemberRow {
    pub relation_osm_id: i64,
    pub way_osm_id: i64,
}

impl CsvRow for MemberRow {
    /// CSV field order matches `MEMBER_COLUMNS`.
    fn csv_fields(self) -> anyhow::Result<Vec<String>> {
        Ok(vec![self.relation_osm_id.to_string(), self.way_osm_id.to_string()])
    }
}

/// Append one CSV record (RFC-4180-ish quoting) to `buf`. Shared by the COPY sink writers and the
/// CSV file writers.
pub fn write_csv_row(buf: &mut Vec<u8>, fields: &[String]) {
    for (i, field) in fields.iter().enumerate() {
        if i > 0 { buf.push(b','); }
        let needs_quoting = field.contains('"') || field.contains(',')
            || field.contains('\n') || field.contains('\\');
        if needs_quoting {
            buf.push(b'"');
            for ch in field.chars() {
                if ch == '"' { buf.extend_from_slice(b"\"\""); }
                else {
                    let mut tmp = [0u8; 4];
                    buf.extend_from_slice(ch.encode_utf8(&mut tmp).as_bytes());
                }
            }
            buf.push(b'"');
        } else {
            buf.extend_from_slice(field.as_bytes());
        }
    }
    buf.push(b'\n');
}
