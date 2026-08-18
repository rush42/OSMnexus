//! Builds one `{table}.parquet` (GeoParquet) file per topic from the same staged output
//! `--output geojson`/`geojsonseq` read (see `output::cursor`'s own doc) — `{table}.bin` tag rows
//! joined forward, in lockstep, to this topic's own `{table}_node_geom`/`{table}_way_geom`/
//! `{table}_relation_geom` tables (or the shared `edges.bin` graph-shape fallback for a topic that
//! declared `"graph": { "way": true }` without its own way geometry table). One merged row per
//! (tag row × geometry row) pair — a topic backed by graph edges duplicates a tag row per split edge
//! segment, same as `output::geojson`, so each Parquet row still carries exactly one geometry
//! payload. `geometry` is plain little-endian WKB (no SRID — the CRS is declared once, in the file's
//! `geo` key-value metadata, per the GeoParquet spec) in WGS84 lon/lat, reprojected from the
//! pipeline's internal Web Mercator by the same `output::cursor::GeomValue` decode step
//! `output::geojson` uses.

use std::collections::{BTreeSet, HashMap};
use std::fs::File;
use std::path::Path;
use std::sync::Arc;

use anyhow::Context;
use arrow_array::builder::{BinaryBuilder, Int64Builder, StringBuilder};
use arrow_array::{ArrayRef, RecordBatch};
use arrow_schema::{DataType, Field, Schema};
use parquet::arrow::ArrowWriter;
use parquet::basic::Compression;
use parquet::file::properties::{EnabledStatistics, WriterProperties};
use parquet::format::KeyValue;
use serde_json::json;

use crate::output::cursor::{group_edges_by_way, read_edges, read_relation_members, EdgeCursor, EdgeGeom, GeomValue, OrderedGeomCursor};
use crate::output::rows::TopicRow;
use crate::output::stage::StageReader;

fn geoparquet_crs84() -> serde_json::Value {
    json!({
        "$schema": "https://proj.org/schemas/v0.5/projjson.schema.json",
        "type": "GeographicCRS",
        "name": "WGS 84 longitude-latitude",
        "datum": {
            "type": "GeodeticReferenceFrame",
            "name": "World Geodetic System 1984",
            "ellipsoid": { "name": "WGS 84", "semi_major_axis": 6378137.0, "inverse_flattening": 298.257223563 }
        },
        "coordinate_system": {
            "subtype": "ellipsoidal",
            "axis": [
                { "name": "Geodetic longitude", "abbreviation": "Lon", "direction": "east", "unit": "degree" },
                { "name": "Geodetic latitude", "abbreviation": "Lat", "direction": "north", "unit": "degree" }
            ]
        },
        "id": { "authority": "OGC", "code": "CRS84" }
    })
}

fn geo_metadata_json(geometry_types: Vec<&str>) -> String {
    json!({
        "version": "1.1.0",
        "primary_column": "geometry",
        "columns": { "geometry": { "encoding": "WKB", "geometry_types": geometry_types, "crs": geoparquet_crs84() } }
    })
    .to_string()
}

/// No `geo` key-value metadata here: it is derived from `geometry_types`, which is only complete
/// once every row has been read, and the writer has to exist *before* that so rows can stream into
/// it. `ArrowWriter::append_key_value_metadata` adds it before `close` instead — see
/// `write_table_parquet`.
fn writer_props() -> WriterProperties {
    WriterProperties::builder()
        .set_compression(Compression::UNCOMPRESSED)
        // The default (`Page`) writes per-page column/offset indexes including size-statistics
        // repetition-level histograms — arrow-rs 53.4.1's writer produces ones pyarrow 19 rejects
        // outright ("Repetition level histogram size mismatch") when reading them back. `Chunk`
        // keeps row-group-level min/max statistics (still useful for query pruning) without the
        // page index that triggers the mismatch.
        .set_statistics_enabled(EnabledStatistics::None)
        .build()
}

fn merged_schema() -> Arc<Schema> {
    Arc::new(Schema::new(vec![
        Field::new("osm_id", DataType::Int64, false),
        Field::new("osm_type", DataType::Utf8, false),
        Field::new("id", DataType::Utf8, true),
        Field::new("category", DataType::Utf8, true),
        Field::new("produced", DataType::Utf8, false),
        Field::new("annotations", DataType::Utf8, false),
        Field::new("meta", DataType::Utf8, false),
        Field::new("geom_type", DataType::Utf8, true),
        Field::new("geometry", DataType::Binary, true),
        Field::new("seg_idx", DataType::Int64, true),
    ]))
}

/// How many merged rows accumulate before being flushed as one `RecordBatch`. Bounds the builders'
/// transient: at germany scale a whole topic's rows are gigabytes, and holding all of them to write
/// a single batch made the builders the largest live allocation in the process. `ArrowWriter`
/// buffers a row group internally and flushes at `max_row_group_size` regardless, so writing many
/// small batches produces the same file as writing one large one — this only changes what is
/// resident while producing it.
const FLUSH_ROWS: usize = 65_536;

/// Column builders for `merged_schema`, flushed to the writer every `FLUSH_ROWS` rows rather than
/// accumulated across the whole topic (which is what the previous single-`RecordBatch` version did,
/// and what its own doc flagged to revisit if output ever showed up as memory-bound — it did).
struct MergedBuilders {
    /// Rows appended since the last flush — `append` is called a variable number of times per tag
    /// row (a graph-fallback row emits one per edge segment), so the caller cannot count them.
    len: usize,
    osm_id: Int64Builder,
    osm_type: StringBuilder,
    id: StringBuilder,
    category: StringBuilder,
    produced: StringBuilder,
    annotations: StringBuilder,
    meta: StringBuilder,
    geom_type: StringBuilder,
    geometry: BinaryBuilder,
    seg_idx: Int64Builder,
}

impl MergedBuilders {
    fn new() -> Self {
        MergedBuilders {
            len: 0,
            osm_id: Int64Builder::new(),
            osm_type: StringBuilder::new(),
            id: StringBuilder::new(),
            category: StringBuilder::new(),
            produced: StringBuilder::new(),
            annotations: StringBuilder::new(),
            meta: StringBuilder::new(),
            geom_type: StringBuilder::new(),
            geometry: BinaryBuilder::new(),
            seg_idx: Int64Builder::new(),
        }
    }

    /// Append one merged row: `row`'s tag fields plus, if `geom`/`seg_idx` are given, this row's
    /// geometry payload (`None` for a tag row with no matching geometry at all — every column but
    /// the tag fields is `null`).
    fn append(&mut self, row: &TopicRow, geom: Option<&GeomValue>, seg_idx: Option<usize>) {
        self.len += 1;
        self.osm_id.append_value(row.osm_id);
        self.osm_type.append_value(row.osm_type);
        match &row.id {
            Some(id) => self.id.append_value(id),
            None => self.id.append_null(),
        }
        match &row.category {
            Some(c) => self.category.append_value(c.as_ref()),
            None => self.category.append_null(),
        }
        self.produced.append_value(&row.produced);
        self.annotations.append_value(&row.annotations);
        self.meta.append_value(&row.meta);
        match geom {
            Some(g) => {
                self.geom_type.append_value(g.geom_type());
                self.geometry.append_value(g.to_wkb());
            }
            None => {
                self.geom_type.append_null();
                self.geometry.append_null();
            }
        }
        match seg_idx {
            Some(idx) => self.seg_idx.append_value(idx as i64),
            None => self.seg_idx.append_null(),
        }
    }

    fn len(&self) -> usize {
        self.len
    }

    /// Take everything appended so far as one `RecordBatch`, leaving the builders empty and reusable
    /// — arrow's own `finish` already resets each column builder, so this needs `&mut self` rather
    /// than consuming, and can be called repeatedly through the row loop.
    fn flush(&mut self, schema: &Arc<Schema>) -> anyhow::Result<RecordBatch> {
        self.len = 0;
        let cols: Vec<ArrayRef> = vec![
            Arc::new(self.osm_id.finish()),
            Arc::new(self.osm_type.finish()),
            Arc::new(self.id.finish()),
            Arc::new(self.category.finish()),
            Arc::new(self.produced.finish()),
            Arc::new(self.annotations.finish()),
            Arc::new(self.meta.finish()),
            Arc::new(self.geom_type.finish()),
            Arc::new(self.geometry.finish()),
            Arc::new(self.seg_idx.finish()),
        ];
        Ok(RecordBatch::try_new(schema.clone(), cols)?)
    }
}

/// Same join pass as `output::geojson::for_each_feature`, but appending into `MergedBuilders`
/// instead of building `serde_json::Value` features — see this module's own doc for the row shape.
fn write_table_parquet(
    out_dir: &Path,
    table: &str,
    edges: &[(i64, EdgeGeom)],
    edges_by_way: &HashMap<i64, Vec<&EdgeGeom>>,
    relation_members: &HashMap<i64, Vec<i64>>,
) -> anyhow::Result<()> {
    let mut node_geom = OrderedGeomCursor::open(&out_dir.join(format!("{table}_node_geom.bin")))?;
    let mut way_geom = OrderedGeomCursor::open(&out_dir.join(format!("{table}_way_geom.bin")))?;
    let mut relation_geom = OrderedGeomCursor::open(&out_dir.join(format!("{table}_relation_geom.bin")))?;
    let mut edge_cursor = EdgeCursor::new(edges);

    let mut tags = StageReader::<TopicRow>::open(&out_dir.join(format!("{table}.bin")))?;
    let mut geometry_types = BTreeSet::new();
    let mut builders = MergedBuilders::new();

    // Writer opened before the row loop so batches can stream into it; the `geo` metadata it still
    // needs is appended after the loop, once `geometry_types` is complete.
    let arrow_schema = merged_schema();
    let path = out_dir.join(format!("{table}.parquet"));
    let file = File::create(&path).with_context(|| format!("creating {}", path.display()))?;
    let mut writer = ArrowWriter::try_new(file, arrow_schema.clone(), Some(writer_props()))?;

    while let Some(row) = tags.next_row()? {
        let osm_id = row.osm_id;

        match row.osm_type {
            "R" => {
                let geom = match relation_geom.as_mut() {
                    Some(cursor) => cursor.get(osm_id)?,
                    None => None,
                };
                if let Some(geom) = geom {
                    geometry_types.insert(geom.geom_type());
                    builders.append(&row, Some(geom), None);
                } else if let Some(way_ids) = relation_members.get(&osm_id) {
                    let segments: Vec<&EdgeGeom> =
                        way_ids.iter().filter_map(|way_id| edges_by_way.get(way_id)).flatten().copied().collect();
                    if segments.is_empty() {
                        builders.append(&row, None, None);
                    }
                    for segment in segments {
                        let g = GeomValue::Line(segment.coordinates.clone());
                        geometry_types.insert(g.geom_type());
                        builders.append(&row, Some(&g), Some(segment.seg_idx));
                    }
                } else {
                    builders.append(&row, None, None);
                }
            }
            "N" => {
                let geom = match node_geom.as_mut() {
                    Some(cursor) => cursor.get(osm_id)?,
                    None => None,
                };
                if let Some(geom) = geom {
                    geometry_types.insert(geom.geom_type());
                }
                builders.append(&row, geom, None);
            }
            _ => {
                let geom = match way_geom.as_mut() {
                    Some(cursor) => cursor.get(osm_id)?,
                    None => None,
                };
                if let Some(geom) = geom {
                    geometry_types.insert(geom.geom_type());
                    builders.append(&row, Some(geom), None);
                } else {
                    let segments = edge_cursor.get_all(osm_id);
                    if segments.is_empty() {
                        builders.append(&row, None, None);
                    }
                    for segment in segments {
                        let g = GeomValue::Line(segment.coordinates.clone());
                        geometry_types.insert(g.geom_type());
                        builders.append(&row, Some(&g), Some(segment.seg_idx));
                    }
                }
            }
        }

        // Flushed between tag rows, never mid-row: a graph-fallback row's edge segments stay in one
        // batch. Row-group boundaries are `ArrowWriter`'s own concern either way.
        if builders.len() >= FLUSH_ROWS {
            writer.write(&builders.flush(&arrow_schema)?)?;
        }
    }

    if builders.len() > 0 {
        writer.write(&builders.flush(&arrow_schema)?)?;
    }

    // Only complete now that every row has been read. `append_key_value_metadata` lands it in the
    // footer exactly as `WriterProperties::set_key_value_metadata` would have.
    let geo_metadata = geo_metadata_json(geometry_types.into_iter().collect::<Vec<_>>());
    writer.append_key_value_metadata(KeyValue { key: "geo".to_owned(), value: Some(geo_metadata) });
    writer.close()?;
    Ok(())
}

/// Reads each of `tables`' staged output (see `output::cursor`'s own doc) and writes one
/// `{table}.parquet` file beside them.
pub fn write_parquet(out_dir: &Path, tables: &[String]) -> anyhow::Result<()> {
    let edges = read_edges(&out_dir.join("edges.bin"))?;
    let edges_by_way = group_edges_by_way(&edges);
    let relation_members = read_relation_members(&out_dir.join("relation_members.bin"))?;

    for table in tables {
        write_table_parquet(out_dir, table, &edges, &edges_by_way, &relation_members)?;
    }
    Ok(())
}
