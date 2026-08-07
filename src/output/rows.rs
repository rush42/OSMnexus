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

/// One column value in Postgres `COPY ... (FORMAT BINARY)` wire representation — see
/// `write_binary_row`'s doc for the framing each variant ends up in. Kept as an enum (rather than
/// writing bytes directly from each row type) so the length-prefix framing lives in one place.
pub enum BinaryField {
    Null,
    /// `bigint` — 8-byte big-endian two's complement.
    Int8(i64),
    /// `integer` — 4-byte big-endian two's complement.
    Int4(i32),
    /// `double precision` — 8-byte big-endian IEEE 754.
    Float8(f64),
    /// `text` — raw UTF-8 bytes, no terminator (the length prefix carries the extent).
    Text(String),
    /// `jsonb` — a 1-byte version prefix (`1`) followed by the UTF-8 JSON text; the version byte
    /// is jsonb's binary-format wire requirement, not part of the JSON itself.
    Jsonb(String),
    /// `geometry` (PostGIS) — raw (E)WKB bytes, which is exactly PostGIS's binary wire format for
    /// the type (its typsend/typrecv are literally WKB/EWKB), so `geom_ewkb` needs no reencoding.
    Bytea(Vec<u8>),
}

/// A row that can be serialized into an ordered list of binary COPY field values. Implemented by
/// every output row type (here and in `geom::rows`) alongside `CsvRow` so `copy_writer` can stream
/// `FORMAT BINARY` instead of text/CSV — see `output::writers::copy_writer`'s doc for why.
pub trait BinaryRow {
    /// The fields in `*_COLUMNS` order, consuming `self` for the same reason `csv_fields` does.
    fn binary_fields(self) -> anyhow::Result<Vec<BinaryField>>;
}

/// The fixed 19-byte `COPY (FORMAT BINARY)` file header: an 11-byte signature, a 4-byte flags
/// field (no bits set — no OIDs), and a 4-byte header-extension length (none).
pub fn write_binary_header(buf: &mut Vec<u8>) {
    buf.extend_from_slice(b"PGCOPY\n\xff\r\n\0");
    buf.extend_from_slice(&0i32.to_be_bytes()); // flags
    buf.extend_from_slice(&0i32.to_be_bytes()); // header extension length
}

/// The 2-byte `COPY (FORMAT BINARY)` file trailer: a field-count of `-1` signals end-of-data.
pub fn write_binary_trailer(buf: &mut Vec<u8>) {
    buf.extend_from_slice(&(-1i16).to_be_bytes());
}

/// Append one binary-format tuple to `buf`: a 2-byte field count, then per field a 4-byte length
/// (or `-1` for NULL) followed by that many bytes of type-specific big-endian data.
pub fn write_binary_row(buf: &mut Vec<u8>, fields: &[BinaryField]) {
    buf.extend_from_slice(&(fields.len() as i16).to_be_bytes());
    for field in fields {
        match field {
            BinaryField::Null => buf.extend_from_slice(&(-1i32).to_be_bytes()),
            BinaryField::Int8(v) => {
                buf.extend_from_slice(&8i32.to_be_bytes());
                buf.extend_from_slice(&v.to_be_bytes());
            }
            BinaryField::Int4(v) => {
                buf.extend_from_slice(&4i32.to_be_bytes());
                buf.extend_from_slice(&v.to_be_bytes());
            }
            BinaryField::Float8(v) => {
                buf.extend_from_slice(&8i32.to_be_bytes());
                buf.extend_from_slice(&v.to_be_bytes());
            }
            BinaryField::Text(s) => {
                buf.extend_from_slice(&(s.len() as i32).to_be_bytes());
                buf.extend_from_slice(s.as_bytes());
            }
            BinaryField::Jsonb(s) => {
                buf.extend_from_slice(&(s.len() as i32 + 1).to_be_bytes());
                buf.push(1u8); // jsonb binary-format version byte
                buf.extend_from_slice(s.as_bytes());
            }
            BinaryField::Bytea(b) => {
                buf.extend_from_slice(&(b.len() as i32).to_be_bytes());
                buf.extend_from_slice(b);
            }
        }
    }
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

impl BinaryRow for TopicRow {
    /// Field order matches `TAG_COLUMNS`; `category` is `Null` (not empty-string) for
    /// `accept_all` rows, since binary COPY has no CSV-style empty-string/NULL ambiguity to lean on.
    fn binary_fields(self) -> anyhow::Result<Vec<BinaryField>> {
        Ok(vec![
            BinaryField::Int8(self.osm_id),
            BinaryField::Text(self.osm_type.to_owned()),
            BinaryField::Text(self.id),
            self.category.map_or(BinaryField::Null, BinaryField::Text),
            BinaryField::Jsonb(self.produced),
            BinaryField::Jsonb(self.annotations),
            BinaryField::Jsonb(self.meta),
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

impl BinaryRow for MemberRow {
    /// Field order matches `MEMBER_COLUMNS`.
    fn binary_fields(self) -> anyhow::Result<Vec<BinaryField>> {
        Ok(vec![BinaryField::Int8(self.relation_osm_id), BinaryField::Int8(self.way_osm_id)])
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
