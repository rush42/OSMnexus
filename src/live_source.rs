//! Alternate element source for the live editor: instead of decoding a PBF file, read ways (tags +
//! already-resolved geometry) straight out of Postgres, filtered to a bbox. Feeds the exact same
//! `TopicRunner::process`/`TableWriters::route_*` machinery the PBF path uses (see `main.rs`) — the
//! only thing that changes is where a way's tags + geometry come from. This only exists because the
//! live editor used to pay for an `osmium extract` + full PBF re-parse on every single edit; with a
//! one-time "all ways" pass (see `configs/live_raw/topic.json`) loading a whole region into Postgres
//! first, a bbox edit becomes one spatial query plus the topic pipeline over however many ways
//! fall inside it.
//!
//! Deliberately way-only, line-geometry-only (no graph/node/relation output) — the live editor only
//! needs to preview filter/producer changes on line geometry; see the caller's own doc for why the
//! routing graph isn't reproduced here.

use std::borrow::Cow;
use std::sync::Arc;

use anyhow::Context;
use rustc_hash::FxHashMap;
use tracing::info;

use crate::config::Config;
use crate::db::pool::build_pool;
use crate::geom::materialize::WayGeometry;
use crate::geom::plan::GeometryPlan;
use crate::geom::rows::WayRow;
use crate::osm::types::RawTags;
use crate::output::types::OsmMeta;
use crate::topic::TopicRunner;

/// Parsed `--bbox min_lon,min_lat,max_lon,max_lat`.
pub struct Bbox {
    pub min_lon: f64,
    pub min_lat: f64,
    pub max_lon: f64,
    pub max_lat: f64,
}

impl Bbox {
    pub fn parse(s: &str) -> anyhow::Result<Self> {
        let parts: Vec<f64> = s
            .split(',')
            .map(|p| p.trim().parse::<f64>().context("bbox component is not a number"))
            .collect::<anyhow::Result<_>>()?;
        anyhow::ensure!(parts.len() == 4, "--bbox needs 4 comma-separated numbers: min_lon,min_lat,max_lon,max_lat");
        Ok(Bbox { min_lon: parts[0], min_lat: parts[1], max_lon: parts[2], max_lat: parts[3] })
    }
}

/// One way as read back from the "all ways" pass's tables: its id, tags (already flattened into
/// `produced` by that pass's `"outputs": true`), and its whole-linestring geometry, already
/// projected to EPSG:3857 and already carrying a valid SRID-flagged EWKB header (`ST_AsEWKB` of a
/// `geometry(LineString,3857)` column round-trips byte-for-byte with `primitives::to_ewkb`'s
/// output), plus its length in metres (computed in SQL — no need to decode the EWKB just to
/// re-derive what Postgres already knows).
struct RawWay {
    osm_id: i64,
    tags: serde_json::Map<String, serde_json::Value>,
    geom_ewkb: Vec<u8>,
    length_m: f64,
}

/// Query every way of `source_table`/`source_table`_geom intersecting `bbox`, in WGS84.
async fn fetch_ways(cfg: &Config, bbox: &Bbox) -> anyhow::Result<Vec<RawWay>> {
    let pool = build_pool(cfg)?;
    let client = pool.get().await.context("connecting to source database")?;

    let geom_table = format!("{}_geom", cfg.source_table);
    let sql = format!(
        "SELECT t.osm_id, t.produced, ST_AsEWKB(g.geom) AS geom_ewkb, ST_Length(g.geom) AS length_m \
         FROM {tags} t \
         JOIN {geom} g ON g.osm_id = t.osm_id \
         WHERE ST_Intersects(g.geom, ST_Transform(ST_MakeEnvelope($1, $2, $3, $4, 4326), 3857))",
        tags = cfg.source_table,
        geom = geom_table,
    );

    let rows = client
        .query(&sql, &[&bbox.min_lon, &bbox.min_lat, &bbox.max_lon, &bbox.max_lat])
        .await
        .with_context(|| format!("querying {} / {geom_table} for bbox", cfg.source_table))?;

    Ok(rows
        .into_iter()
        .map(|row| {
            let produced: serde_json::Value = row.get("produced");
            let tags = produced.as_object().cloned().unwrap_or_default();
            RawWay {
                osm_id: row.get("osm_id"),
                tags,
                geom_ewkb: row.get("geom_ewkb"),
                length_m: row.get("length_m"),
            }
        })
        .collect())
}

/// Run the live-editor topic set (`runners`) over every way in `bbox`, routing tag + line-geometry
/// rows into `writers` exactly like the PBF path's select/materialize phases do — minus node
/// resolution (the geometry is already known) and minus the routing graph/relations/nodes passes
/// (out of scope for the live editor today; see this module's own doc).
pub async fn run(
    cfg: &Config,
    runners: Arc<Vec<TopicRunner>>,
    plan: Arc<GeometryPlan>,
    writers: Arc<crate::output::sinks::TableWriters>,
) -> anyhow::Result<usize> {
    let bbox_str = cfg.bbox.as_deref().context("--source postgis requires --bbox")?;
    let bbox = Bbox::parse(bbox_str)?;

    let t0 = std::time::Instant::now();
    let ways = fetch_ways(cfg, &bbox).await?;
    info!("Fetched {} ways from {} in {:.2}s", ways.len(), cfg.source_table, t0.elapsed().as_secs_f32());

    let n = ways.len();
    let runners_c = runners.clone();
    let plan_c = plan.clone();
    let writers_c = writers.clone();
    tokio::task::spawn_blocking(move || {
        let no_meta = OsmMeta { updated_at: None, updated_by: None, changeset_id: None };
        for way in ways {
            let mut tags: RawTags = FxHashMap::default();
            for (k, v) in &way.tags {
                let s = match v {
                    serde_json::Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                tags.insert(Cow::Owned(k.clone()), Cow::Owned(s));
            }

            let mut mask = 0u32;
            let topic_rows: Vec<Vec<crate::output::rows::TopicRow>> = runners_c
                .iter()
                .enumerate()
                .map(|(i, r)| {
                    let rows = r.process(crate::osm::types::ElementKind::Way, way.osm_id, &tags, &no_meta);
                    if !rows.is_empty() {
                        mask |= 1 << i;
                    }
                    rows
                })
                .collect();

            let kept = writers_c.route_tag(topic_rows);
            if kept {
                let line = WayRow { osm_id: way.osm_id, geom_ewkb: way.geom_ewkb, length_m: way.length_m };
                let geometry = WayGeometry { edges: None, line: Some(line), point: None, polygon: None };
                writers_c.route_way(mask, geometry, &plan_c);
            }
        }
    })
    .await
    .context("postgis select-phase task panicked")?;

    Ok(n)
}
