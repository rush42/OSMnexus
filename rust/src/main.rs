mod classify;
mod config;
mod db;
mod error;
mod osm;
mod output;
mod transform;

use anyhow::Context;
use clap::Parser;
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
    let ways = read_highway_ways(&cfg.pbf_file)?;
    info!("{} highway ways loaded", ways.len());

    let transformations = default_transformations();
    let mut bikelane_rows: Vec<BikelaneRow> = Vec::new();
    let mut road_rows: Vec<RoadRow> = Vec::new();

    info!("Processing ways...");
    for way in &ways {
        let mut tags = way.tags.clone();

        // Tag transformations (order matters)
        transform_lifecycle_tags(&mut tags);
        transform_cycleway_opposite_schema(&mut tags);
        transform_construction_prefix(&mut tags);
        transform_cycleway_both_postfix(&mut tags);

        if should_exclude(&tags) {
            continue;
        }

        let coords = &way.coords;
        let length_m = haversine_length_m(coords);
        let geom = project_line(coords);

        let meta = OsmMeta {
            updated_at: way.meta.timestamp.map(|ts| {
                chrono::DateTime::from_timestamp(ts, 0)
                    .map(|dt| dt.format("%Y-%m-%dT%H:%M:%SZ").to_string())
                    .unwrap_or_default()
            }),
            updated_by: way.meta.user.clone(),
            changeset_id: way.meta.changeset,
        };

        // Road row
        if !by_access_bikelanes(&tags) && !by_service(&tags) {
            if let Some(road) = road_classification_value(&tags) {
                let highway = tags.get("highway").cloned().unwrap_or_default();
                let id = format!("way/{}", way.id);
                let minzoom = road_minzoom(&road);

                let osm_tags = RoadOsmTags {
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
                };

                let derived = RoadDerived {
                    id: id.clone(),
                    road,
                    length_m,
                    lifecycle: tags.get("lifecycle").cloned(),
                    bikelane_left: None,
                    bikelane_right: None,
                    bikelane_self: None,
                };

                road_rows.push(RoadRow {
                    osm_id: way.id,
                    osm_type: "W",
                    id,
                    osm: osm_tags,
                    derived,
                    meta: OsmMeta {
                        updated_at: meta.updated_at.clone(),
                        updated_by: meta.updated_by.clone(),
                        changeset_id: meta.changeset_id,
                    },
                    geom: geom.clone(),
                    minzoom,
                });
            }
        }

        // Bikelane rows: split into transformed objects
        let transformed = get_transformed_objects(&tags, &transformations);

        for obj in &transformed {
            let ctx = CategoryContext {
                tags: &obj.tags,
                side: obj.side,
                prefix: obj.prefix,
                parent_highway: obj.parent_highway.as_deref(),
                parent_tags: if obj.parent_highway.is_some() { Some(&tags) } else { None },
                infix: None, // simplified: full infix tracking would require storing it in TransformedObject
                length_m,
            };

            let Some(category) = categorize_bikelane(&ctx) else {
                continue;
            };

            if !category.infrastructure_exists {
                continue;
            }

            let id = match obj.side {
                Side::Self_ => format!("way/{}", way.id),
                Side::Left => format!("way/{}/{}/left", way.id, obj.prefix.unwrap_or("cycleway")),
                Side::Right => format!("way/{}/{}/right", way.id, obj.prefix.unwrap_or("cycleway")),
            };

            let osm_tags = BikelaneOsmTags {
                name: obj.tags.get("name").or_else(|| tags.get("name")).cloned(),
                surface: obj.tags.get("surface").or_else(|| tags.get("surface")).cloned(),
                smoothness: obj.tags.get("smoothness").or_else(|| tags.get("smoothness")).cloned(),
                width: obj
                    .tags
                    .get("width")
                    .and_then(|v| v.parse::<f32>().ok()),
                source_width: obj.tags.get("source:width").cloned(),
                bridge: tags.get("bridge").map(|v| v == "yes"),
                tunnel: tags.get("tunnel").map(|v| v == "yes"),
                oneway: obj.tags.get("oneway").cloned(),
                oneway_bicycle: obj
                    .tags
                    .get("oneway:bicycle")
                    .or_else(|| tags.get("oneway:bicycle"))
                    .cloned(),
                traffic_sign: obj.tags.get("traffic_sign").cloned(),
                informal: tags.get("informal").map(|v| v == "yes"),
                covered: tags.get("covered").map(|v| v == "yes"),
                operator_type: tags.get("operator_type").cloned(),
                mapillary: tags.get("mapillary").cloned(),
                segregated: obj.tags.get("segregated").cloned(),
                bicycle: obj.tags.get("bicycle").cloned(),
                foot: obj.tags.get("foot").cloned(),
            };

            let derived = BikelaneDerived {
                id: id.clone(),
                category: category.id,
                side: obj.side,
                prefix: obj.prefix,
                parent_highway: obj.parent_highway.clone(),
                road: road_classification_value(&tags),
                length_m,
                lifecycle: tags.get("lifecycle").cloned(),
            };

            bikelane_rows.push(BikelaneRow {
                osm_id: way.id,
                osm_type: "W",
                id,
                osm: osm_tags,
                derived,
                meta: OsmMeta {
                    updated_at: meta.updated_at.clone(),
                    updated_by: meta.updated_by.clone(),
                    changeset_id: meta.changeset_id,
                },
                geom: geom.clone(),
                minzoom: bikelane_minzoom(length_m),
            });
        }
    }

    info!(
        "Processed: {} bikelane rows, {} road rows",
        bikelane_rows.len(),
        road_rows.len()
    );

    info!("Writing bikelanes to DB...");
    let n = writer::write_bikelanes(&client, &bikelane_rows).await?;
    info!("Wrote {} bikelane rows", n);

    info!("Writing roads to DB...");
    let n = writer::write_roads(&client, &road_rows).await?;
    info!("Wrote {} road rows", n);

    info!("Creating indexes...");
    schema::create_indexes(&client).await?;

    info!("Done.");
    Ok(())
}
