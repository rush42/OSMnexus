use osm_bikelanes::{classify, config, db, osm, output, transform};

use anyhow::Context;
use bytes::Bytes;
use clap::Parser;
use futures::SinkExt;
use rayon::prelude::*;
use tracing::info;

use classify::{
    bikelane_categories::{categorize_bikelane, CategoryContext},
    exclude::{by_access_bikelanes, by_service, should_exclude},
    minzoom::{bikelane_minzoom, road_minzoom},
    road_classification::road_classification_value,
    sanitize::{self as san},
};
use config::Config;
use db::{pool::build_pool, schema, writer};
use osm::reader::read_highway_ways;
use output::{
    bikelane_row::BikelaneRow,
    geometry::{haversine_length_m, project_line},
    road_row::RoadRow,
    types::{
        BikelaneDerived, BikelaneOsm, BikelanePrivate, BikelaneSanitized,
        OsmMeta, RoadDerived, RoadOsm, RoadPrivate, RoadSanitized, Side,
    },
};
use transform::{
    construction_prefix::transform_construction_prefix,
    cycleway_both::transform_cycleway_both_postfix,
    lifecycle::transform_lifecycle_tags,
    opposite::transform_cycleway_opposite_schema,
    side_split::{default_transformations, get_transformed_objects},
};

struct WayOutput {
    bikelane_rows: Vec<BikelaneRow>,
    road_row: Option<RoadRow>,
}

const FLUSH_BYTES: usize = 512 * 1024;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt::init();

    let cfg = Config::parse();

    info!("Connecting to database {}@{}/{}", cfg.db_user, cfg.db_host, cfg.db_name);
    let pool = build_pool(&cfg)?;
    let client_setup = pool.get().await.context("getting DB connection")?;

    info!("Setting up schema...");
    schema::create_tables(&client_setup).await?;
    if cfg.truncate {
        schema::truncate_tables(&client_setup).await?;
    }
    schema::drop_indexes(&client_setup).await?;
    drop(client_setup);

    info!("Reading PBF: {}", cfg.pbf_file);
    let t0 = std::time::Instant::now();
    let ways = read_highway_ways(&cfg.pbf_file)?;
    info!("{} highway ways loaded in {:.1}s", ways.len(), t0.elapsed().as_secs_f32());

    info!("Processing ways (streaming to DB)...");
    let t1 = std::time::Instant::now();
    let transformations = default_transformations();

    let (tx, mut rx) = tokio::sync::mpsc::channel::<WayOutput>(512);

    let process_task = tokio::task::spawn_blocking(move || {
        ways.par_iter().for_each(|way| {
            let output = process_way(way, &transformations);
            let _ = tx.blocking_send(output);
        });
    });

    let client_bl = pool.get().await.context("getting bikelane DB connection")?;
    let client_rd = pool.get().await.context("getting road DB connection")?;

    let bikelane_sink = client_bl.copy_in(writer::COPY_BIKELANES).await?;
    let road_sink     = client_rd.copy_in(writer::COPY_ROADS).await?;
    let mut bikelane_sink = std::pin::pin!(bikelane_sink);
    let mut road_sink     = std::pin::pin!(road_sink);

    let mut bl_buf: Vec<u8> = Vec::with_capacity(FLUSH_BYTES);
    let mut rd_buf: Vec<u8> = Vec::with_capacity(FLUSH_BYTES);
    let mut bl_count = 0usize;
    let mut rd_count = 0usize;

    while let Some(output) = rx.recv().await {
        for row in output.bikelane_rows {
            writer::write_bikelane_csv_row(&mut bl_buf, &row)?;
            bl_count += 1;
            if bl_buf.len() >= FLUSH_BYTES {
                bikelane_sink.send(Bytes::from(std::mem::take(&mut bl_buf))).await?;
                bl_buf = Vec::with_capacity(FLUSH_BYTES);
            }
        }
        if let Some(row) = output.road_row {
            writer::write_road_csv_row(&mut rd_buf, &row)?;
            rd_count += 1;
            if rd_buf.len() >= FLUSH_BYTES {
                road_sink.send(Bytes::from(std::mem::take(&mut rd_buf))).await?;
                rd_buf = Vec::with_capacity(FLUSH_BYTES);
            }
        }
    }

    if !bl_buf.is_empty() { bikelane_sink.send(Bytes::from(bl_buf)).await?; }
    if !rd_buf.is_empty() { road_sink.send(Bytes::from(rd_buf)).await?; }
    bikelane_sink.finish().await?;
    road_sink.finish().await?;

    process_task.await.context("rayon processing panicked")?;

    info!(
        "Wrote {} bikelane rows, {} road rows in {:.1}s",
        bl_count, rd_count, t1.elapsed().as_secs_f32(),
    );

    info!("Creating indexes...");
    let client_idx = pool.get().await.context("getting index DB connection")?;
    schema::create_indexes(&client_idx).await?;

    info!("Done. Total: {:.1}s", t0.elapsed().as_secs_f32());
    Ok(())
}

fn process_way(
    way: &osm::types::OsmWay,
    transformations: &[transform::side_split::CenterLineTransformation],
) -> WayOutput {
    let mut tags = way.tags.clone();

    transform_lifecycle_tags(&mut tags);
    transform_cycleway_opposite_schema(&mut tags);
    transform_construction_prefix(&mut tags);
    transform_cycleway_both_postfix(&mut tags);

    if should_exclude(&tags) {
        return WayOutput { bikelane_rows: Vec::new(), road_row: None };
    }

    let length_m = haversine_length_m(&way.coords);
    let geom = project_line(&way.coords);

    let meta = OsmMeta {
        updated_at: way.meta.timestamp.and_then(|ts| {
            chrono::DateTime::from_timestamp(ts, 0)
                .map(|dt: chrono::DateTime<chrono::Utc>| dt.format("%Y-%m-%dT%H:%M:%SZ").to_string())
        }),
        updated_by: way.meta.user.clone(),
        changeset_id: way.meta.changeset,
    };

    let road_row = build_road_row(way, &tags, &geom, length_m, &meta);
    let bikelane_rows = build_bikelane_rows(way, &tags, transformations, &geom, length_m, &meta);

    WayOutput { bikelane_rows, road_row }
}

fn build_road_row(
    way: &osm::types::OsmWay,
    tags: &output::types::RawTagsRef,
    geom: &geo::LineString<f64>,
    length_m: f64,
    meta: &OsmMeta,
) -> Option<RoadRow> {
    if by_access_bikelanes(tags) || by_service(tags) { return None; }
    let road = road_classification_value(tags)?;
    let highway = tags.get("highway").cloned().unwrap_or_default();
    let id = format!("way/{}", way.id);
    let minzoom = road_minzoom(&road);

    fn raw(tags: &output::types::RawTagsRef, key: &str) -> Option<String> {
        tags.get(key).cloned()
    }

    Some(RoadRow {
        osm_id: way.id,
        osm_type: "W",
        id: id.clone(),
        osm: RoadOsm {
            highway,
            name:          raw(tags, "name"),
            name_ref:      raw(tags, "ref"),
            surface:       raw(tags, "surface"),
            smoothness:    raw(tags, "smoothness"),
            maxspeed:      raw(tags, "maxspeed"),
            oneway:        raw(tags, "oneway"),
            oneway_bicycle: raw(tags, "oneway:bicycle"),
            lit:           raw(tags, "lit"),
            bridge:        raw(tags, "bridge"),
            tunnel:        raw(tags, "tunnel"),
            operator_type: raw(tags, "operator_type"),
            informal:      raw(tags, "informal"),
            covered:       raw(tags, "covered"),
            traffic_sign:  raw(tags, "traffic_sign"),
        },
        sanitized: RoadSanitized {
            bridge:       san::sanitize_yes_flag(tags, "bridge"),
            tunnel:       san::sanitize_yes_flag(tags, "tunnel"),
            traffic_sign: raw(tags, "traffic_sign").as_deref().and_then(san::sanitize_traffic_sign),
        },
        derived: RoadDerived {
            id,
            road,
            length_m,
            lifecycle: raw(tags, "lifecycle"),
            bikelane_left: None,
            bikelane_right: None,
            bikelane_self: None,
        },
        private: RoadPrivate {},
        meta: meta.clone(),
        geom: geom.clone(),
        minzoom,
    })
}

fn build_bikelane_rows(
    way: &osm::types::OsmWay,
    tags: &output::types::RawTagsRef,
    transformations: &[transform::side_split::CenterLineTransformation],
    geom: &geo::LineString<f64>,
    length_m: f64,
    meta: &OsmMeta,
) -> Vec<BikelaneRow> {
    let transformed = get_transformed_objects(tags, transformations);
    let mut rows = Vec::new();

    for obj in &transformed {
        let parent_tags = obj.parent_highway.as_ref().map(|_| tags);
        let ctx = CategoryContext {
            tags: &obj.tags,
            side: obj.side,
            prefix: obj.prefix,
            parent_highway: obj.parent_highway.as_deref(),
            parent_tags,
            infix: obj.infix,
            length_m,
        };

        let Some(category) = categorize_bikelane(&ctx) else { continue };
        if !category.infrastructure_exists { continue }

        let id = match obj.side {
            Side::Self_ => format!("way/{}", way.id),
            Side::Left  => format!("way/{}/{}/left",  way.id, obj.prefix.unwrap_or("cycleway")),
            Side::Right => format!("way/{}/{}/right", way.id, obj.prefix.unwrap_or("cycleway")),
        };

        let otags = &obj.tags;

        fn raw(t: &output::types::RawTagsRef, key: &str) -> Option<String> {
            t.get(key).cloned()
        }

        // surface / smoothness with category-based parent fallback
        let copy_from_parent = category.copy_surface_smoothness_from_parent;
        let surface = raw(otags, "surface")
            .or_else(|| if copy_from_parent { raw(tags, "surface") } else { None });
        let smoothness = raw(otags, "smoothness")
            .or_else(|| if copy_from_parent { raw(tags, "smoothness") } else { None });

        // lifecycle: temporary flag takes priority, then tag
        let lifecycle = san::temporary(otags)
            .map(str::to_owned)
            .or_else(|| raw(otags, "lifecycle"))
            .or_else(|| raw(tags, "lifecycle"));

        let osm = BikelaneOsm {
            name:             raw(otags, "name").or_else(|| raw(tags, "name")),
            surface:          raw(otags, "surface"),
            smoothness:       raw(otags, "smoothness"),
            width:            raw(otags, "width"),
            source_width:     raw(otags, "source:width"),
            bridge:           raw(tags, "bridge"),
            tunnel:           raw(tags, "tunnel"),
            oneway:           raw(otags, "oneway"),
            oneway_bicycle:   raw(otags, "oneway:bicycle").or_else(|| raw(tags, "oneway:bicycle")),
            traffic_sign:     raw(otags, "traffic_sign"),
            informal:         raw(tags, "informal"),
            covered:          raw(tags, "covered"),
            operator_type:    raw(tags, "operator_type"),
            mapillary:        raw(tags, "mapillary"),
            segregated:       raw(otags, "segregated"),
            bicycle:          raw(otags, "bicycle"),
            foot:             raw(otags, "foot"),
            description:      raw(otags, "description"),
            note:             raw(otags, "note"),
            temporary:        raw(otags, "temporary"),
            separation_left:  raw(otags, "separation:left"),
            separation_right: raw(otags, "separation:right"),
            separation_both:  raw(otags, "separation:both"),
            marking_left:     raw(otags, "marking:left"),
            marking_right:    raw(otags, "marking:right"),
            marking_both:     raw(otags, "marking:both"),
            traffic_mode_left:  raw(otags, "traffic_mode:left"),
            traffic_mode_right: raw(otags, "traffic_mode:right"),
            traffic_mode_both:  raw(otags, "traffic_mode:both"),
            buffer_left:      raw(otags, "buffer:left"),
            buffer_right:     raw(otags, "buffer:right"),
            buffer_both:      raw(otags, "buffer:both"),
            surface_colour:   raw(otags, "surface:colour").or_else(|| raw(otags, "surface:color")),
        };

        let sanitized = BikelaneSanitized {
            traffic_sign:     raw(otags, "traffic_sign").as_deref().and_then(san::sanitize_traffic_sign),
            separation_left:  san::separation(otags, "left"),
            separation_right: san::separation(otags, "right"),
            marking_left:     san::marking(otags, "left"),
            marking_right:    san::marking(otags, "right"),
            traffic_mode_left:  san::traffic_mode(otags, "left"),
            traffic_mode_right: san::traffic_mode(otags, "right"),
            buffer_left:      san::buffer(otags, "left"),
            buffer_right:     san::buffer(otags, "right"),
            surface_color:    san::surface_color(otags),
            bridge:           san::sanitize_yes_flag(tags, "bridge"),
            tunnel:           san::sanitize_yes_flag(tags, "tunnel"),
            oneway:           san::derive_oneway(otags, category),
            width:            raw(otags, "width").as_deref().and_then(san::parse_length),
            width_effective:  raw(otags, "width:effective").as_deref().and_then(san::parse_length),
            lifecycle,
            surface,
            smoothness,
        };

        let derived = BikelaneDerived {
            id: id.clone(),
            category: category.id.as_str(),
            road: road_classification_value(tags),
            length_m,
        };

        let private = BikelanePrivate {
            side:   obj.side,
            prefix: obj.prefix,
            infix:  obj.infix,
            parent_highway: obj.parent_highway.clone(),
            implicit_oneway_confidence: category.implicit_oneway_confidence.as_str(),
        };

        rows.push(BikelaneRow {
            osm_id: way.id,
            osm_type: "W",
            id,
            osm,
            sanitized,
            derived,
            private,
            meta: meta.clone(),
            geom: geom.clone(),
            minzoom: bikelane_minzoom(length_m),
        });
    }
    rows
}
