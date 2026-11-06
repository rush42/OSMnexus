//! Non-geometry output row types (tag rows + relation-member links), and the encodings every row
//! type here and in `geom::rows` implements. Geometry row types (`EdgeRow`/`GeomRow`/`NodeRow`) live
//! in `geom::rows` instead — see `geom`'s own module doc for why geometry is split out.
//!
//! The contract between the pipeline and a writer is the **row struct itself** — `TopicRow`,
//! `MemberRow`, and `geom::rows`' three — routed to whichever sink the run's `--output` selected.
//! Each sink owns its own encoding of that struct, and nothing else has to know about it:
//!
//!   * [`BinaryRow`] → Postgres `COPY ... (FORMAT BINARY)` wire bytes, used *only* by
//!     `sinks::copy_writer`. It used to be the one canonical encoding every sink went through,
//!     including the staged files the file backends read back after a run — which made Postgres's
//!     wire format a contract on paths that never touch Postgres. See `output::stage`'s own doc for
//!     what that cost.
//!   * [`CsvRow`] → CSV text fields, used by `sinks::csv_writer`. Previously "stringify each
//!     `BinaryField`", so CSV output was shaped by Postgres's type model for no reason.
//!   * `output::stage`'s `StageRow`/`StageDecode` → the run's own staging files, which the
//!     `geojson`/`geojsonseq`/`parquet` backends read back (`output::cursor`).
//!   * `sinks::memory_sink` → no encoding at all; the structs go straight to an in-process consumer.
//!
//! Adding a backend means adding an encoding of the struct, not a decoding of somebody else's.

/// Column lists shared by the COPY statement and the CSV header line (no spaces → valid as both).
/// The field order here **must** match each row type's `binary_fields`/`csv_fields` implementations
/// below.
pub const TAG_COLUMNS: &str = "osm_id,osm_type,id,category,produced,annotations,meta";
/// `TAG_COLUMNS` without `id`, for a topic that set `"id_type": "none"` (see `TopicSpec::id_type`).
/// The column set is fixed per table, so every row of that table omits the field and the COPY
/// statement/CSV header must agree — binary COPY encodes a field count per tuple and rejects a
/// mismatch outright. (The staging format has no such constraint: it writes `id` as an explicit
/// optional, so a staged row is self-describing — see `output::stage`.)
pub const TAG_COLUMNS_NO_ID: &str = "osm_id,osm_type,category,produced,annotations,meta";

/// The tag-table column list for a topic, given whether it emits the `id` column.
pub fn tag_columns(emits_id: bool) -> &'static str {
    if emits_id {
        TAG_COLUMNS
    } else {
        TAG_COLUMNS_NO_ID
    }
}
pub const MEMBER_COLUMNS: &str = "relation_osm_id,way_osm_id";

/// One column value in Postgres `COPY ... (FORMAT BINARY)` wire representation — see
/// `write_binary_row`'s doc for the framing each variant ends up in. Kept as an enum (rather than
/// writing bytes directly from each row type) so the length-prefix framing lives in one place.
/// Postgres-only: nothing outside `sinks::copy_writer` consumes these.
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
    /// `text` from a shared handle — for a value interned once at load and reused by many rows
    /// (a category id), so the row carries a refcount rather than its own copy.
    TextShared(std::sync::Arc<str>),
    /// `jsonb` — a 1-byte version prefix (`1`) followed by the UTF-8 JSON text; the version byte
    /// is jsonb's binary-format wire requirement, not part of the JSON itself.
    Jsonb(String),
    /// `geometry` (PostGIS) — raw (E)WKB bytes, which is exactly PostGIS's binary wire format for
    /// the type (its typsend/typrecv are literally WKB/EWKB), so `geom_ewkb` needs no reencoding.
    Bytea(Vec<u8>),
}

/// A row that can be serialized into an ordered list of binary COPY field values, in `*_COLUMNS`
/// order — the `--output pg` encoding, consumed by `sinks::copy_writer` alone. Consuming `self`
/// lets `TopicRow` move its already-JSON-encoded `produced`/`annotations`/`meta` `String`s straight
/// out instead of cloning them.
pub trait BinaryRow {
    fn binary_fields(self) -> anyhow::Result<Vec<BinaryField>>;
}

/// A row that can be written as a CSV record, in `*_COLUMNS` order — the `--output csv` encoding,
/// consumed by `sinks::csv_writer` alone. Appends into a caller-owned buffer so the writer can
/// reuse one `Vec` across every row of a table instead of allocating per row.
///
/// An unset value (`category` on an `accept_all` row, `length_m` on a point, an omitted `meta`) is
/// an empty field — the same convention Postgres's `COPY ... FORMAT CSV` reads back as `NULL`.
pub trait CsvRow {
    fn csv_fields(&self, out: &mut Vec<String>);
}

/// Every encoding a sink might ask a row for. `sinks::Shard` picks its writer task from the run's
/// `--output` at runtime, so a row type routed through one has to satisfy all of them — this is the
/// bound that says so once instead of at each use site. Blanket-implemented: implementing the three
/// encodings is all it takes.
pub trait OutputRow: BinaryRow + CsvRow + crate::output::stage::StageRow + Send + Sync + 'static {}
impl<T: BinaryRow + CsvRow + crate::output::stage::StageRow + Send + Sync + 'static> OutputRow for T {}

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
            BinaryField::TextShared(s) => {
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
    /// `None` for a topic that set `"id_type": "none"` — the field is then omitted from the `pg`/
    /// `csv` column set entirely, matching `TAG_COLUMNS_NO_ID`.
    pub id: Option<String>,
    /// The matched category's id — a dedicated column rather than a `produced` key, since every
    /// row has at most one and it's not itself a `Producer`-evaluated output. `None` for an
    /// `accept_all` kind (see `TopicSpec::accept_all`), which never matches a category at all.
    /// Shared rather than owned: one handle per distinct category id, interned when the topic loads
    /// and re-interned by `output::stage::Interner` when a staged row is read back.
    pub category: Option<std::sync::Arc<str>>,
    /// Every non-underscore-prefixed output (the former separate `osm`/`sanitized`/`derived`
    /// columns — all three were always the same `Producer`-evaluation mechanism, just different
    /// JSON shorthands for declaring one entry in one `producers` map; see `TopicSpec::producers`).
    /// Pre-serialized to its final JSON text by `topic::pipeline::build_topic_rows`, which runs on
    /// the rayon classify workers (up to 8-way parallel here) — not left as a `Map` for a writer
    /// task to serialize later, which would cap JSON encoding at `--db-writers`-way parallelism
    /// (4 by default) regardless of how many workers did the classifying. It stays text all the way
    /// through: `pg` wants jsonb, `csv` and `parquet` want the string verbatim, and only `geojson`
    /// parses it (to merge into a feature's `properties`).
    pub produced: String,
    /// Engine-attached bookkeeping about `produced`, not itself a topic-authored output: side-split
    /// context (`_side`/`_prefix`/`_infix`, stamped by `topic::pipeline::build_topic_rows`/each `Clone`)
    /// and each output's companion `annotate` provenance (`<output>_source`/`<output>_confidence`,
    /// from `Produced::annotate`) — see `topic::pipeline::eval_fields`. Pre-serialized, same reason
    /// as `produced`.
    pub annotations: String,
    pub meta: String,
}

/// A jsonb field, or SQL `NULL` when the row left it empty — an omitted `meta` (see
/// `TopicSpec::meta`) or a base object's dropped `annotations`. `NULL` rather than `{}` because it
/// is both smaller on disk and the honest representation: the value wasn't empty, it wasn't
/// recorded.
fn jsonb_or_null(s: String) -> BinaryField {
    if s.is_empty() {
        BinaryField::Null
    } else {
        BinaryField::Jsonb(s)
    }
}

/// `osm_type`'s only three possible values are the `&'static str` constants `ElementKind::osm_type`
/// hands out — decoding maps a staged string back onto one of those instead of leaking an owned
/// `String` into a field declared `&'static str`.
fn osm_type_from_str(s: &str) -> anyhow::Result<&'static str> {
    match s {
        "N" => Ok("N"),
        "W" => Ok("W"),
        "R" => Ok("R"),
        other => anyhow::bail!("unknown osm_type {other:?}"),
    }
}

impl BinaryRow for TopicRow {
    /// Field order matches `TAG_COLUMNS`; `category` is `Null` (not empty-string) for
    /// `accept_all` rows, since binary COPY has no CSV-style empty-string/NULL ambiguity to lean on.
    fn binary_fields(self) -> anyhow::Result<Vec<BinaryField>> {
        let mut fields =
            vec![BinaryField::Int8(self.osm_id), BinaryField::Text(self.osm_type.to_owned())];
        if let Some(id) = self.id {
            fields.push(BinaryField::Text(id));
        }
        fields.extend([
            self.category.map_or(BinaryField::Null, BinaryField::TextShared),
            jsonb_or_null(self.produced),
            jsonb_or_null(self.annotations),
            jsonb_or_null(self.meta),
        ]);
        Ok(fields)
    }
}

impl CsvRow for TopicRow {
    /// Field order matches `TAG_COLUMNS`/`TAG_COLUMNS_NO_ID` — `id` is skipped entirely (not
    /// emitted empty) when absent, since the header this row is written under omits the column too.
    fn csv_fields(&self, out: &mut Vec<String>) {
        out.push(self.osm_id.to_string());
        out.push(self.osm_type.to_owned());
        if let Some(id) = &self.id {
            out.push(id.clone());
        }
        out.push(self.category.as_deref().unwrap_or_default().to_owned());
        out.push(self.produced.clone());
        out.push(self.annotations.clone());
        out.push(self.meta.clone());
    }
}

impl crate::output::stage::StageRow for TopicRow {
    fn stage_encode(&self, buf: &mut Vec<u8>) {
        use crate::output::stage::{put_i64, put_opt_str, put_str};
        put_i64(buf, self.osm_id);
        put_str(buf, self.osm_type);
        put_opt_str(buf, self.id.as_deref());
        put_opt_str(buf, self.category.as_deref());
        put_str(buf, &self.produced);
        put_str(buf, &self.annotations);
        put_str(buf, &self.meta);
    }
}

impl crate::output::stage::StageDecode for TopicRow {
    /// Interns `category`: a topic has a handful of distinct category ids across tens of millions of
    /// rows, so decoding shares one `Arc<str>` per value rather than allocating a `String` per row.
    type Ctx = crate::output::stage::Interner;

    fn stage_decode(
        cur: &mut crate::output::stage::StageCursor<'_>,
        ctx: &mut Self::Ctx,
    ) -> anyhow::Result<Self> {
        let osm_id = cur.i64()?;
        let osm_type = osm_type_from_str(cur.str()?)?;
        let id = cur.opt_str()?.map(str::to_owned);
        let category = cur.opt_str()?.map(|s| ctx.intern(s));
        let produced = cur.str()?.to_owned();
        let annotations = cur.str()?.to_owned();
        let meta = cur.str()?.to_owned();
        Ok(TopicRow { osm_id, osm_type, id, category, produced, annotations, meta })
    }
}

/// A relation → member-way link row, emitted for every way member of a kept relation. Lets a
/// relation's tag row be joined downstream to the geometries of its member ways (relations have no
/// materialized geometry of their own).
pub struct MemberRow {
    pub relation_osm_id: i64,
    pub way_osm_id: i64,
}

impl BinaryRow for MemberRow {
    /// Field order matches `MEMBER_COLUMNS`.
    fn binary_fields(self) -> anyhow::Result<Vec<BinaryField>> {
        Ok(vec![BinaryField::Int8(self.relation_osm_id), BinaryField::Int8(self.way_osm_id)])
    }
}

impl CsvRow for MemberRow {
    fn csv_fields(&self, out: &mut Vec<String>) {
        out.push(self.relation_osm_id.to_string());
        out.push(self.way_osm_id.to_string());
    }
}

impl crate::output::stage::StageRow for MemberRow {
    fn stage_encode(&self, buf: &mut Vec<u8>) {
        use crate::output::stage::put_i64;
        put_i64(buf, self.relation_osm_id);
        put_i64(buf, self.way_osm_id);
    }
}

impl crate::output::stage::StageDecode for MemberRow {
    type Ctx = ();

    fn stage_decode(
        cur: &mut crate::output::stage::StageCursor<'_>,
        _: &mut (),
    ) -> anyhow::Result<Self> {
        Ok(MemberRow { relation_osm_id: cur.i64()?, way_osm_id: cur.i64()? })
    }
}

/// Append one CSV record (RFC-4180-ish quoting) to `buf`. Shared by every CSV file writer.
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output::stage::{StageCursor, StageDecode, StageRow};
    use std::sync::Arc;

    fn stage_round_trip<T: StageRow + StageDecode>(row: &T) -> T {
        let mut buf = Vec::new();
        row.stage_encode(&mut buf);
        let mut cur = StageCursor::new(&buf);
        T::stage_decode(&mut cur, &mut <T as StageDecode>::Ctx::default()).unwrap()
    }

    fn csv_of<T: CsvRow>(row: &T) -> Vec<String> {
        let mut out = Vec::new();
        row.csv_fields(&mut out);
        out
    }

    fn topic_row(id: Option<String>, category: Option<&str>) -> TopicRow {
        TopicRow {
            osm_id: 42,
            osm_type: "W",
            id,
            category: category.map(Arc::from),
            produced: r#"{"highway":"primary"}"#.to_owned(),
            annotations: String::new(),
            meta: r#"{"source":"osm"}"#.to_owned(),
        }
    }

    #[test]
    fn topic_row_stage_round_trips_with_id_and_category() {
        let decoded = stage_round_trip(&topic_row(Some("way/42".to_owned()), Some("road")));
        assert_eq!(decoded.osm_id, 42);
        assert_eq!(decoded.osm_type, "W");
        assert_eq!(decoded.id.as_deref(), Some("way/42"));
        assert_eq!(decoded.category.as_deref(), Some("road"));
        assert_eq!(decoded.produced, r#"{"highway":"primary"}"#);
        assert_eq!(decoded.annotations, "");
        assert_eq!(decoded.meta, r#"{"source":"osm"}"#);
    }

    #[test]
    fn topic_row_stage_round_trips_without_id_or_category() {
        // Unlike the Postgres wire format this replaced, an absent `id` is a value in the row, not
        // a missing column the reader has to infer from a field count.
        let decoded = stage_round_trip(&topic_row(None, None));
        assert_eq!(decoded.id, None);
        assert_eq!(decoded.category, None);
        assert_eq!(decoded.osm_id, 42);
    }

    #[test]
    fn member_row_stage_round_trips() {
        let decoded = stage_round_trip(&MemberRow { relation_osm_id: 7, way_osm_id: 99 });
        assert_eq!(decoded.relation_osm_id, 7);
        assert_eq!(decoded.way_osm_id, 99);
    }

    #[test]
    fn topic_row_csv_fields_match_the_column_list() {
        // accept_all row (category = None) with an id.
        let csv = csv_of(&topic_row(Some("way/42".to_owned()), None));
        assert_eq!(
            csv,
            vec![
                "42".to_owned(),
                "W".to_owned(),
                "way/42".to_owned(),
                String::new(), // accept_all category -> empty field
                r#"{"highway":"primary"}"#.to_owned(),
                String::new(), // empty annotations -> empty field, not "null"
                r#"{"source":"osm"}"#.to_owned(),
            ]
        );
        assert_eq!(csv.len(), TAG_COLUMNS.split(',').count());
    }

    #[test]
    fn topic_row_csv_fields_omit_id_when_the_topic_has_none() {
        let csv = csv_of(&topic_row(None, Some("road")));
        assert_eq!(csv.len(), TAG_COLUMNS_NO_ID.split(',').count());
        assert_eq!(csv[2], "road"); // category slides into `id`'s place, as the header says
    }

    #[test]
    fn member_row_csv_fields_match_the_column_list() {
        let csv = csv_of(&MemberRow { relation_osm_id: 7, way_osm_id: 99 });
        assert_eq!(csv, vec!["7".to_owned(), "99".to_owned()]);
        assert_eq!(csv.len(), MEMBER_COLUMNS.split(',').count());
    }
}
