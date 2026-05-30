use osm_bikelanes::{classify, config, db, error, osm, output, transform};

use anyhow::Context;
use clap::Parser;
use rayon::prelude::*;
use tracing::info;

use classify::{
    bikelane_categories::{categorize_bikelane, CategoryContext},
    exclude::{by_access_bikelanes, by_service, should_exclude},
    minzoom::{bikelane_minzoom, road_minzoom},
    road_classification::road_classification_value,
};
use config::Config;
use db::{pool::build_pool, schema, writer};
use osm::reader::read_highway_ways;
use output::{
    bikelane_row::BikelaneRow,
    geometry::{haversine_length_m, project_line},
    road_row::RoadRow,
    types::{BikelaneDerived, BikelaneOsmTags, OsmMeta, RoadDerived, RoadOsmTags, Side},
};
use transform::{
    construction_prefix::transform_construction_prefix,
    cycleway_both::transform_cycleway_both_postfix,
    lifecycle::transform_lifecycle_tags,
    opposite::transform_cycleway_opposite_schema,
    side_split::{default_transformations, get_transformed_objects},
};

/// Output of processing one OSM way.
struct WayOutput {
    bikelane_rows: Vec<BikelaneRow>,
    road_row: Option<RoadRow>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt::init();

    let cfg = Config::parse();

    info!("Connecting to database {}@{}/{}", cfg.db_user, cfg.db_host, cfg.db_name);
    let pool = build_pool(&cfg)?;
    let client = pool.get().await.context("getting DB connection")?;

    info!("Setting up schema...");
    schema::create_tables(&client).await?;
    if cfg.truncate {
        schema::truncate_tables(&client).await?;
    }
    schema::drop_indexes(&client).await?;

    info!("Reading PBF: {}", cfg.pbf_file);
    let t0 = std::time::Instant::now();
    let ways = read_highway_ways(&cfg.pbf_file)?;
    info!("{} highway ways loaded in {:.1}s", ways.len(), t0.elapsed().as_secs_f32());

    info!("Processing ways (parallel)...");
    let t1 = std::time::Instant::now();
    let transformations = default_transformations();

    // Process all ways in parallel with rayon, collect (bikelane_rows, road_row) per way.
    let outputs: Vec<WayOutput> = ways
        .par_iter()
        .map(|way| process_way(way, &transformations))
        .collect();

    // Flatten results.
    let mut bikelane_rows: Vec<BikelaneRow> = Vec::new();
    let mut road_rows: Vec<RoadRow> = Vec::new();
    for output in outputs {
        bikelane_rows.extend(output.bikelane_rows);
        if let Some(r) = output.road_row {
            road_rows.push(r);
        }
    }

    info!(
        "Processed: {} bikelane rows, {} road rows in {:.1}s",
        bikelane_rows.len(),
        road_rows.len(),
        t1.elapsed().as_secs_f32(),
    );

    info!("Writing bikelanes to DB...");
    let t2 = std::time::Instant::now();
    let n = writer::write_bikelanes(&client, &bikelane_rows).await?;
    info!("Wrote {} bikelane rows in {:.1}s", n, t2.elapsed().as_secs_f32());

    info!("Writing roads to DB...");
    let t3 = std::time::Instant::now();
    let n = writer::write_roads(&client, &road_rows).await?;
    info!("Wrote {} road rows in {:.1}s", n, t3.elapsed().as_secs_f32());

    info!("Creating indexes...");
    schema::create_indexes(&client).await?;

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

    // Road row
    let road_row = build_road_row(way, &tags, &geom, length_m, &meta);

    // Bikelane rows
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
    if by_access_bikelanes(tags) || by_service(tags) {
        return None;
    }
    let road = road_classification_value(tags)?;
    let highway = tags.get("highway").cloned().unwrap_or_default();
    let id = format!("way/{}", way.id);
    let minzoom = road_minzoom(&road);

    Some(RoadRow {
        osm_id: way.id,
        osm_type: "W",
        id: id.clone(),
        osm: RoadOsmTags {
            highway,
            name: tags.get("name").cloned(),
            name_ref: tags.get("ref").cloned(),
            surface: tags.get("surface").cloned(),
            smoothness: tags.get("smoothness").cloned(),
            maxspeed: tags.get("maxspeed").cloned(),
            oneway: tags.get("oneway").cloned(),
            oneway_bicycle: tags.get("oneway:bicycle").cloned(),
            lit: tags.get("lit").cloned(),
            bridge: tags.get("bridge").map(|v| v == "yes"),
            tunnel: tags.get("tunnel").map(|v| v == "yes"),
            operator_type: tags.get("operator_type").cloned(),
            informal: tags.get("informal").map(|v| v == "yes"),
            covered: tags.get("covered").map(|v| v == "yes"),
            traffic_sign: tags.get("traffic_sign").cloned(),
        },
        derived: RoadDerived {
            id,
            road,
            length_m,
            lifecycle: tags.get("lifecycle").cloned(),
            bikelane_left: None,
            bikelane_right: None,
            bikelane_self: None,
        },
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

        let copy_from_parent = category.copy_surface_smoothness_from_parent;
        let surface = obj.tags.get("surface")
            .or_else(|| if copy_from_parent { tags.get("surface") } else { None })
            .cloned();
        let smoothness = obj.tags.get("smoothness")
            .or_else(|| if copy_from_parent { tags.get("smoothness") } else { None })
            .cloned();

        rows.push(BikelaneRow {
            osm_id: way.id,
            osm_type: "W",
            id: id.clone(),
            osm: BikelaneOsmTags {
                name: obj.tags.get("name").or_else(|| tags.get("name")).cloned(),
                surface,
                smoothness,
                width: obj.tags.get("width").and_then(|v| v.parse::<f32>().ok()),
                source_width: obj.tags.get("source:width").cloned(),
                bridge: tags.get("bridge").map(|v| v == "yes"),
                tunnel: tags.get("tunnel").map(|v| v == "yes"),
                oneway: obj.tags.get("oneway").cloned(),
                oneway_bicycle: obj.tags.get("oneway:bicycle")
                    .or_else(|| tags.get("oneway:bicycle")).cloned(),
                traffic_sign: obj.tags.get("traffic_sign").cloned(),
                informal: tags.get("informal").map(|v| v == "yes"),
                covered: tags.get("covered").map(|v| v == "yes"),
                operator_type: tags.get("operator_type").cloned(),
                mapillary: tags.get("mapillary").cloned(),
                segregated: obj.tags.get("segregated").cloned(),
                bicycle: obj.tags.get("bicycle").cloned(),
                foot: obj.tags.get("foot").cloned(),
            },
            derived: BikelaneDerived {
                id,
                category: category.id.as_str(),
                side: obj.side,
                prefix: obj.prefix,
                parent_highway: obj.parent_highway.clone(),
                road: road_classification_value(tags),
                length_m,
                lifecycle: tags.get("lifecycle").cloned(),
            },
            meta: meta.clone(),
            geom: geom.clone(),
            minzoom: bikelane_minzoom(length_m),
        });
    }
    rows
}
