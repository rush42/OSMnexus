//! Non-geometry output row types (tag rows + relation-member links), and the shared
//! `BinaryField`/`BinaryRow` encoding + `write_csv_row`/`read_binary_row` helpers both this module
//! and `geom::rows` use. Geometry row types (`EdgeRow`/`GeomRow`/`NodeRow`) live in `geom::rows`
//! instead — see `geom`'s own module doc for why geometry is split out.
//!
//! `BinaryField` (Postgres `COPY ... FORMAT BINARY`'s wire representation) is the *one* canonical
//! column encoding every row type implements (`BinaryRow`) — not one of two competing encodings.
//! Text CSV output is just "stringify each `BinaryField`" (`binary_fields_to_csv_row`, one function,
//! generic over every row type), and the disk-staged binary format `geojson`/`geojsonseq` read back
//! after a run is the same wire bytes `--output pg` already streams to Postgres, just written to a
//! file instead of a live connection — `read_binary_row`/`FromBinaryRow` are that decode path. See
//! `output::sinks`' own doc for how a row's single `binary_fields()` call feeds all of `pg`/`csv`/
//! `geojson`/`geojsonseq`'s sinks.

use anyhow::Context;

/// Column lists shared by the COPY statement and the CSV header line (no spaces → valid as both).
/// The field order here **must** match each row type's `binary_fields` implementation below.
pub const TAG_COLUMNS: &str = "osm_id,osm_type,id,category,produced,annotations,meta";
/// `TAG_COLUMNS` without `id`, for a topic that set `"id_type": "none"` (see `TopicSpec::id_type`).
/// The column set is fixed per table, so every row of that table omits the field and the COPY
/// statement/CSV header must agree — binary COPY encodes a field count per tuple and rejects a
/// mismatch outright.
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
/// writing bytes directly from each row type) so the length-prefix framing lives in one place, and
/// so the exact same value doubles as `read_binary_row`'s decoded output and
/// `binary_fields_to_csv_row`'s text-formatting input — one typed intermediate serving every sink.
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
    /// (a category id), so the row carries a refcount rather than its own copy. Decoding never
    /// produces this variant (the wire bytes don't preserve which strings were shared) — only
    /// `Text` — but a `FromBinaryRow` impl is free to re-intern into an `Arc<str>` itself.
    TextShared(std::sync::Arc<str>),
    /// `jsonb` — a 1-byte version prefix (`1`) followed by the UTF-8 JSON text; the version byte
    /// is jsonb's binary-format wire requirement, not part of the JSON itself.
    Jsonb(String),
    /// `geometry` (PostGIS) — raw (E)WKB bytes, which is exactly PostGIS's binary wire format for
    /// the type (its typsend/typrecv are literally WKB/EWKB), so `geom_ewkb` needs no reencoding.
    Bytea(Vec<u8>),
}

/// A row that can be serialized into an ordered list of binary COPY field values — the one encoding
/// every output row type (here and in `geom::rows`) implements; every sink (`pg`, `csv`, `geojson`/
/// `geojsonseq` staging) starts from this same `binary_fields()` call. See `output::sinks`' own doc.
pub trait BinaryRow {
    /// The fields in `*_COLUMNS` order, consuming `self` — lets `TopicRow` move its
    /// already-JSON-encoded `produced`/`annotations`/`meta` `String`s straight out instead of
    /// cloning them (no sink needs the row again after this call).
    fn binary_fields(self) -> anyhow::Result<Vec<BinaryField>>;
}

/// The reverse of [`BinaryRow`] — reconstructs a row from `read_binary_row`'s decoded fields. Only
/// implemented for row types actually read back after a run (`TopicRow`/`GeomRow`/`EdgeRow`, for
/// `geojson`/`geojsonseq`'s post-run cursor join — see `output::geojson`); `pg`/`csv`/in-memory sinks
/// are one-pass writers that never re-read their own output, so `NodeRow`/`MemberRow` don't need it
/// (`MemberRow` does anyway, for `read_relation_members`'s small hashmap build).
pub trait FromBinaryRow: Sized {
    fn from_binary_fields(fields: Vec<BinaryField>) -> anyhow::Result<Self>;
}

/// A `BinaryField`'s type, without its value — the schema `read_binary_row` needs to know before it
/// can decode a row, since (like Postgres's own COPY BINARY consumer) the wire format carries a
/// length-prefixed byte blob per field but no type tag; see `write_binary_row`'s doc. One entry per
/// `*_COLUMNS` column, in that order.
#[derive(Clone, Copy)]
pub enum BinaryFieldType {
    Int8,
    Int4,
    Float8,
    Text,
    Jsonb,
    Bytea,
}

/// `TAG_COLUMNS`/`TAG_COLUMNS_NO_ID`'s field types, for `TopicRow::from_binary_fields`'s caller to
/// pass to `read_binary_row` — mirrors `tag_columns`'s `emits_id` conditional.
pub fn tag_binary_schema(emits_id: bool) -> Vec<BinaryFieldType> {
    let mut schema = vec![BinaryFieldType::Int8, BinaryFieldType::Text];
    if emits_id {
        schema.push(BinaryFieldType::Text);
    }
    schema.extend([BinaryFieldType::Text, BinaryFieldType::Jsonb, BinaryFieldType::Jsonb, BinaryFieldType::Jsonb]);
    schema
}

/// `MEMBER_COLUMNS`'s field types.
pub fn member_binary_schema() -> Vec<BinaryFieldType> {
    vec![BinaryFieldType::Int8, BinaryFieldType::Int8]
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

/// Read + validate the fixed 19-byte binary-format file header, advancing `r` past it — the reverse
/// of `write_binary_header`. Called once per file, before any `read_binary_row` call.
pub fn read_binary_header<R: std::io::Read>(r: &mut R) -> anyhow::Result<()> {
    let mut header = [0u8; 19];
    r.read_exact(&mut header).context("reading binary file header")?;
    anyhow::ensure!(&header[0..11] == b"PGCOPY\n\xff\r\n\0", "bad binary file signature");
    Ok(())
}

/// Read one binary-format tuple from `r` — the reverse of `write_binary_row`. `schema` gives each
/// field's expected type in `*_COLUMNS` order (the wire format carries no type tag of its own, only
/// a length prefix — see `write_binary_row`'s doc, and `BinaryFieldType`'s own doc for why a schema
/// has to come from the caller). Returns `None` at the `-1` field-count trailer (end of data) — the
/// caller stops reading. Generic over `Read` rather than a fixed `&[u8]` slice so the same decode
/// logic serves both a small file read fully into memory (`relation_members.bin`, `edges.bin`) and a
/// `BufReader` streamed straight off disk (`{table}.bin}`, which can be gigabytes for a
/// country-sized run — see `output::geojson`'s own doc).
pub fn read_binary_row<R: std::io::Read>(r: &mut R, schema: &[BinaryFieldType]) -> anyhow::Result<Option<Vec<BinaryField>>> {
    let mut count_buf = [0u8; 2];
    match r.read_exact(&mut count_buf) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e).context("reading binary row field count"),
    }
    let field_count = i16::from_be_bytes(count_buf);
    if field_count == -1 {
        return Ok(None);
    }
    anyhow::ensure!(
        field_count as usize == schema.len(),
        "binary row field count {field_count} doesn't match schema length {}",
        schema.len()
    );
    let mut fields = Vec::with_capacity(schema.len());
    for ty in schema {
        let mut len_buf = [0u8; 4];
        r.read_exact(&mut len_buf).context("reading binary field length")?;
        let len = i32::from_be_bytes(len_buf);
        if len == -1 {
            fields.push(BinaryField::Null);
            continue;
        }
        let len = len as usize;
        let mut bytes = vec![0u8; len];
        r.read_exact(&mut bytes).context("reading binary field bytes")?;
        fields.push(match ty {
            BinaryFieldType::Int8 => BinaryField::Int8(i64::from_be_bytes(bytes[..].try_into()?)),
            BinaryFieldType::Int4 => BinaryField::Int4(i32::from_be_bytes(bytes[..].try_into()?)),
            BinaryFieldType::Float8 => BinaryField::Float8(f64::from_be_bytes(bytes[..].try_into()?)),
            BinaryFieldType::Text => BinaryField::Text(String::from_utf8(bytes)?),
            BinaryFieldType::Jsonb => {
                anyhow::ensure!(!bytes.is_empty(), "empty jsonb field (missing version byte)");
                BinaryField::Jsonb(String::from_utf8(bytes[1..].to_vec())?)
            }
            BinaryFieldType::Bytea => BinaryField::Bytea(bytes),
        });
    }
    Ok(Some(fields))
}

/// Stringify a row's binary fields into CSV text, in the same order — the single function every row
/// type's CSV output now goes through, replacing what used to be a hand-written `csv_fields` impl
/// per type. `Null`/empty-string are the same thing here (matches every prior `csv_fields`
/// convention: an unset `category`/`length_m`/etc. was already an empty CSV field, read back as
/// `NULL` by Postgres `COPY ... FORMAT CSV`).
pub fn binary_fields_to_csv_row(fields: Vec<BinaryField>) -> Vec<String> {
    fields
        .into_iter()
        .map(|f| match f {
            BinaryField::Null => String::new(),
            BinaryField::Int8(v) => v.to_string(),
            BinaryField::Int4(v) => v.to_string(),
            BinaryField::Float8(v) => v.to_string(),
            BinaryField::Text(s) => s,
            BinaryField::TextShared(s) => s.to_string(),
            BinaryField::Jsonb(s) => s,
            BinaryField::Bytea(b) => hex::encode(b),
        })
        .collect()
}

/// A single tag row produced by the topic engine — one per (way, side, prefix), independent of
/// how the geometry is later cut. Geometry lives in the paired geom table (see `EdgeRow`), joined
/// on `osm_id` at tile-materialization time.
pub struct TopicRow {
    pub osm_id: i64,
    pub osm_type: &'static str,
    /// `None` for a topic that set `"id_type": "none"` — the field is then omitted from the row
    /// entirely, matching `TAG_COLUMNS_NO_ID`.
    pub id: Option<String>,
    /// The matched category's id — a dedicated column rather than a `produced` key, since every
    /// row has at most one and it's not itself a `Producer`-evaluated output. `None` for an
    /// `accept_all` kind (see `TopicSpec::accept_all`), which never matches a category at all;
    /// serializes as `NULL` in binary, which `binary_fields_to_csv_row` turns into an empty CSV
    /// field the same way it always was.
    pub category: Option<std::sync::Arc<str>>,
    /// Every non-underscore-prefixed output (the former separate `osm`/`sanitized`/`derived`
    /// columns — all three were always the same `Producer`-evaluation mechanism, just different
    /// JSON shorthands for declaring one entry in one `producers` map; see `TopicSpec::producers`).
    /// Pre-serialized to its final JSON text by `topic::pipeline::build_topic_rows`, which runs on
    /// the rayon classify workers (up to 8-way parallel here) — not left as a `Map` for a writer
    /// task to serialize later, which would cap JSON encoding at `--db-writers`-way parallelism
    /// (4 by default) regardless of how many workers did the classifying.
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
/// hands out — decoding maps a wire string back onto one of those instead of leaking an owned
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

impl FromBinaryRow for TopicRow {
    /// Reverses `binary_fields` — `fields.len()` (6 vs 7) tells whether the `id` column is present,
    /// same distinction `tag_binary_schema`'s `emits_id` makes on the encode side.
    fn from_binary_fields(fields: Vec<BinaryField>) -> anyhow::Result<Self> {
        anyhow::ensure!(fields.len() == 6 || fields.len() == 7, "unexpected tag row field count {}", fields.len());
        let emits_id = fields.len() == 7;
        let mut it = fields.into_iter();
        let osm_id = match it.next() {
            Some(BinaryField::Int8(v)) => v,
            _ => anyhow::bail!("tag row: expected osm_id (Int8)"),
        };
        let osm_type = match it.next() {
            Some(BinaryField::Text(s)) => osm_type_from_str(&s)?,
            _ => anyhow::bail!("tag row: expected osm_type (Text)"),
        };
        let id = if emits_id {
            match it.next() {
                Some(BinaryField::Text(s)) => Some(s),
                _ => anyhow::bail!("tag row: expected id (Text)"),
            }
        } else {
            None
        };
        let category = match it.next() {
            Some(BinaryField::Text(s)) => Some(std::sync::Arc::from(s.as_str())),
            Some(BinaryField::Null) => None,
            _ => anyhow::bail!("tag row: expected category (Text or Null)"),
        };
        let mut jsonb_field = || -> anyhow::Result<String> {
            match it.next() {
                Some(BinaryField::Jsonb(s)) => Ok(s),
                Some(BinaryField::Null) => Ok(String::new()),
                _ => anyhow::bail!("tag row: expected jsonb field"),
            }
        };
        let produced = jsonb_field()?;
        let annotations = jsonb_field()?;
        let meta = jsonb_field()?;
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

impl FromBinaryRow for MemberRow {
    fn from_binary_fields(fields: Vec<BinaryField>) -> anyhow::Result<Self> {
        anyhow::ensure!(fields.len() == 2, "unexpected member row field count {}", fields.len());
        let mut it = fields.into_iter();
        let relation_osm_id = match it.next() {
            Some(BinaryField::Int8(v)) => v,
            _ => anyhow::bail!("member row: expected relation_osm_id (Int8)"),
        };
        let way_osm_id = match it.next() {
            Some(BinaryField::Int8(v)) => v,
            _ => anyhow::bail!("member row: expected way_osm_id (Int8)"),
        };
        Ok(MemberRow { relation_osm_id, way_osm_id })
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn round_trip<T: BinaryRow + FromBinaryRow>(row: T, schema: &[BinaryFieldType]) -> Vec<BinaryField> {
        let fields = row.binary_fields().unwrap();
        let mut buf = Vec::new();
        write_binary_row(&mut buf, &fields);
        let mut cursor = std::io::Cursor::new(buf.as_slice());
        read_binary_row(&mut cursor, schema).unwrap().unwrap()
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
    fn topic_row_round_trips_with_id_and_category() {
        let row = topic_row(Some("way/42".to_owned()), Some("road"));
        let decoded = TopicRow::from_binary_fields(round_trip(row, &tag_binary_schema(true))).unwrap();
        assert_eq!(decoded.osm_id, 42);
        assert_eq!(decoded.osm_type, "W");
        assert_eq!(decoded.id.as_deref(), Some("way/42"));
        assert_eq!(decoded.category.as_deref(), Some("road"));
        assert_eq!(decoded.produced, r#"{"highway":"primary"}"#);
        assert_eq!(decoded.annotations, "");
        assert_eq!(decoded.meta, r#"{"source":"osm"}"#);
    }

    #[test]
    fn topic_row_round_trips_without_id_or_category() {
        let row = topic_row(None, None);
        let decoded = TopicRow::from_binary_fields(round_trip(row, &tag_binary_schema(false))).unwrap();
        assert_eq!(decoded.id, None);
        assert_eq!(decoded.category, None);
    }

    #[test]
    fn member_row_round_trips() {
        let row = MemberRow { relation_osm_id: 7, way_osm_id: 99 };
        let decoded = MemberRow::from_binary_fields(round_trip(row, &member_binary_schema())).unwrap();
        assert_eq!(decoded.relation_osm_id, 7);
        assert_eq!(decoded.way_osm_id, 99);
    }

    #[test]
    fn binary_fields_to_csv_row_matches_prior_csv_fields_convention() {
        // accept_all row (category = None) with an id — mirrors the old `csv_fields` shape.
        let row = topic_row(Some("way/42".to_owned()), None);
        let csv = binary_fields_to_csv_row(row.binary_fields().unwrap());
        assert_eq!(
            csv,
            vec![
                "42".to_owned(),
                "W".to_owned(),
                "way/42".to_owned(),
                String::new(), // accept_all category -> empty field, same as before
                r#"{"highway":"primary"}"#.to_owned(),
                String::new(), // empty annotations -> empty field, not "null"
                r#"{"source":"osm"}"#.to_owned(),
            ]
        );
    }

    #[test]
    fn read_binary_row_stops_at_trailer() {
        let mut buf = Vec::new();
        write_binary_trailer(&mut buf);
        let mut cursor = std::io::Cursor::new(buf.as_slice());
        assert!(read_binary_row(&mut cursor, &member_binary_schema()).unwrap().is_none());
    }
}
