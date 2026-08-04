//! Alternate element source for the live editor: instead of decoding a PBF file, classify elements
//! from their tags alone, read as CSV from stdin — **no geometry, ever**. Feeds the exact same
//! `TopicRunner::process`/`TableWriters::route_tag` machinery the PBF path uses (see `main.rs`) for
//! tag classification; unlike the PBF path (and unlike this module's previous incarnation,
//! `live_source.rs`), it never touches `route_way` or any geometry table, so `--source csv` only
//! ever makes sense with `--output csv` (enforced in `main.rs`) — there's nothing to build a
//! `geojsonseq` Feature stream out of.
//!
//! Which elements to select (bbox query, way-id search, ...) and how to turn the classification
//! result back into renderable features is deliberately *not* this module's concern — that's the
//! caller's job. The live editor (`editor/lib/liveEditor.ts`) already holds each element's WGS84
//! geometry from the same query it used to build this CSV; it joins the classified `<topic>.csv` tag
//! rows this run produces back to that geometry itself, entirely client-side. This keeps the CSV
//! wire format genuinely general — `osm_id,osm_type,tags_json` classifies against *any*
//! tagged-entity dataset, not just OSM elements with EWKB geometry attached — and means geometry
//! (the expensive part to move across a process boundary; see the earlier `--source postgis`/hex-EWKB
//! version of this module for why) never crosses the pipeline/caller boundary at all for this path.
//!
//! CSV schema (no header row), one line per element: `osm_id,osm_type,tags_json`, where `osm_type`
//! is `ElementKind::osm_type()`'s own `N`/`W`/`R` and `tags_json` is a JSON object of the element's
//! already-resolved tags (e.g. Postgres's `produced` column, `::text`). A relation row classifies
//! against its own tags exactly like a node/way row does — this module never touches
//! `relation_members` or any member-way geometry, that's `fetchFeatures`' job, entirely on the
//! `editor/lib/liveEditor.ts` side (see its own doc for how it approximates a relation's shape).

use std::borrow::Cow;
use std::sync::Arc;

use anyhow::Context;
use rustc_hash::FxHashMap;
use tracing::info;

use crate::osm::types::{ElementKind, RawTags};
use crate::output::types::OsmMeta;
use crate::topic::TopicRunner;

/// One element as read back off stdin: its id, kind, and tags (already flattened by the caller's
/// selection query) — see this module's own doc for the CSV schema.
struct RawElement {
    osm_id: i64,
    kind: ElementKind,
    tags: serde_json::Map<String, serde_json::Value>,
}

fn parse_osm_type(s: &str) -> anyhow::Result<ElementKind> {
    match s {
        "N" => Ok(ElementKind::Node),
        "W" => Ok(ElementKind::Way),
        "R" => Ok(ElementKind::Relation),
        other => anyhow::bail!("unknown osm_type {other:?} (expected N/W/R)"),
    }
}

/// Reads `osm_id,osm_type,tags_json` rows from `r`, one element per line.
fn read_elements<R: std::io::Read>(r: R) -> anyhow::Result<Vec<RawElement>> {
    let mut reader = csv::ReaderBuilder::new().has_headers(false).from_reader(r);
    let mut elements = Vec::new();
    for result in reader.records() {
        let record = result.context("reading a CSV element row from stdin")?;
        anyhow::ensure!(record.len() == 3, "expected 3 CSV fields (osm_id,osm_type,tags_json), got {}", record.len());
        let osm_id: i64 = record[0].parse().with_context(|| format!("invalid osm_id {:?}", &record[0]))?;
        let kind = parse_osm_type(&record[1]).with_context(|| format!("element {osm_id}"))?;
        let tags_value: serde_json::Value =
            serde_json::from_str(&record[2]).with_context(|| format!("invalid tags_json for element {osm_id}"))?;
        let tags = tags_value.as_object().cloned().unwrap_or_default();
        elements.push(RawElement { osm_id, kind, tags });
    }
    Ok(elements)
}

/// Run the live-editor topic set (`runners`) over every element read from stdin, routing tag rows
/// into `writers` exactly like the PBF path's select phase does — minus everything geometry-related
/// (node resolution, way/edge materialization) since there is none here; see this module's own doc.
pub async fn run(runners: Arc<Vec<TopicRunner>>, writers: Arc<crate::output::sinks::TableWriters>) -> anyhow::Result<usize> {
    let t0 = std::time::Instant::now();
    let elements = tokio::task::spawn_blocking(|| read_elements(std::io::stdin().lock()))
        .await
        .context("csv stdin read task panicked")??;
    info!("Read {} elements from stdin in {:.2}s", elements.len(), t0.elapsed().as_secs_f32());

    let n = elements.len();
    tokio::task::spawn_blocking(move || {
        let no_meta = OsmMeta { updated_at: None, updated_by: None, changeset_id: None };
        for element in elements {
            let mut tags: RawTags = FxHashMap::default();
            for (k, v) in &element.tags {
                let s = match v {
                    serde_json::Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                tags.insert(Cow::Owned(k.clone()), Cow::Owned(s));
            }

            let topic_rows: Vec<Vec<crate::output::rows::TopicRow>> =
                runners.iter().map(|r| r.process(element.kind, element.osm_id, &tags, &no_meta)).collect();
            writers.route_tag(topic_rows);
        }
    })
    .await
    .context("csv classify task panicked")?;

    Ok(n)
}
