use anyhow::Context;
use bytes::Bytes;
use futures::SinkExt;
use tokio_postgres::Client;

use crate::classify::categories::{load_categories_from_dir, load_shared_macros, CategoriesFile};
use crate::engine::{runner::{build_topic_rows, TopicRow}, topic::TopicSpec};
use crate::osm::types::{OsmWay, RawTags};
use crate::output::types::OsmMeta;
use crate::transform::side_split::CenterLineTransformation;

const COPY_SQL: &str =
    "(osm_id, osm_type, id, osm, sanitized, derived, private, meta, geom, minzoom) FROM STDIN (FORMAT CSV)";

/// A fully loaded topic ready to process ways.
pub struct TopicRunner {
    pub spec: TopicSpec,
    pub categories: CategoriesFile,
    pub transformations: Vec<CenterLineTransformation>,
}

impl TopicRunner {
    /// Load a topic from its directory under `topics/<name>/`.
    pub fn load(name: &str) -> anyhow::Result<Self> {
        let base = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("topics/{name}"));

        let spec: TopicSpec = serde_json::from_str(
            &std::fs::read_to_string(base.join("topic.json"))
                .with_context(|| format!("reading topics/{name}/topic.json"))?,
        )
        .with_context(|| format!("parsing topics/{name}/topic.json"))?;

        let cats_dir = base.join("categories");
        let mut categories = if cats_dir.exists() {
            load_categories_from_dir(&cats_dir)
                .with_context(|| format!("loading topics/{name}/categories/"))?
        } else {
            // No categories/ dir → nothing matches, so the topic emits no rows.
            // Every shipped topic has a categories/ dir; this is just a safe fallback.
            CategoriesFile { macros: Default::default(), categories: Vec::new() }
        };

        // Merge shared cross-topic macros (topics/_shared/) into this topic's macro
        // namespace. Topic-local macros win on name conflict.
        let shared_dir = base.parent().expect("topics/<name> has a parent").join("_shared");
        for (k, v) in load_shared_macros(&shared_dir)
            .with_context(|| "loading topics/_shared/")?
        {
            categories.macros.entry(k).or_insert(v);
        }

        let transformations = spec
            .transformations
            .iter()
            .map(|t| CenterLineTransformation {
                highway: Box::leak(t.highway.clone().into_boxed_str()),
                prefix:  Box::leak(t.prefix.clone().into_boxed_str()),
            })
            .collect();

        Ok(Self { spec, categories, transformations })
    }

    pub fn table(&self) -> &str {
        &self.spec.table
    }

    pub fn copy_sql(&self) -> String {
        format!("COPY {} {COPY_SQL}", self.spec.table)
    }

    pub fn process(&self, way: &OsmWay, tags: &RawTags, geom: &geo::LineString<f64>, length_m: f64, meta: &OsmMeta) -> Vec<TopicRow> {
        build_topic_rows(&self.spec, &self.categories, way, tags, &self.transformations, geom, length_m, meta)
    }
}

const FLUSH_BYTES: usize = 512 * 1024;

/// Write a slice of TopicRows into any COPY sink, flushing every 512 KB.
pub async fn stream_rows<S>(
    rows: Vec<TopicRow>,
    buf: &mut Vec<u8>,
    mut sink: std::pin::Pin<&mut S>,
) -> anyhow::Result<usize>
where
    S: futures::Sink<Bytes, Error = tokio_postgres::Error>,
{
    let mut n = 0;
    for row in rows {
        let fields = row.to_csv_fields()?;
        write_csv_row(buf, &fields);
        n += 1;
        if buf.len() >= FLUSH_BYTES {
            sink.as_mut().send(Bytes::from(std::mem::take(buf))).await?;
            *buf = Vec::with_capacity(FLUSH_BYTES);
        }
    }
    Ok(n)
}

fn write_csv_row(buf: &mut Vec<u8>, fields: &[String]) {
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

/// Open a COPY sink for a topic on a dedicated connection.
pub async fn open_copy_sink(
    client: &Client,
    copy_sql: &str,
) -> anyhow::Result<tokio_postgres::CopyInSink<Bytes>> {
    Ok(client.copy_in(copy_sql).await?)
}
