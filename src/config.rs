use std::path::PathBuf;

use clap::Parser;

#[derive(Parser, Debug)]
#[command(about = "Data-driven OSM PBF topic processing pipeline → PostgreSQL")]
pub struct Config {
    /// Path to the .osm.pbf file
    #[arg(env = "PBF_FILE")]
    pub pbf_file: String,

    /// Config directory: a self-contained folder of topic dirs plus a `_shared/` library
    /// (macros, sanitizers, value_sets.json, classifiers/). Selects which set of topics to run,
    /// e.g. `configs/tilda`, `configs/example`, `configs/public_transport`.
    #[arg(long, default_value = "configs/tilda")]
    pub config_dir: PathBuf,

    /// DB host; leave empty to connect via Unix socket (peer auth)
    #[arg(long, env = "PGHOST", default_value = "")]
    pub db_host: String,

    #[arg(long, env = "PGDATABASE", default_value = "postgres")]
    pub db_name: String,

    #[arg(long, env = "PGUSER", default_value = "postgres")]
    pub db_user: String,

    #[arg(long, env = "PGPASSWORD", default_value = "")]
    pub db_password: String,

    #[arg(long, env = "PGPORT", default_value_t = 5432)]
    pub db_port: u16,

    /// Truncate tables before import
    #[arg(long, default_value_t = true)]
    pub truncate: bool,

    /// Size of the rayon thread pool used for the CPU-bound PBF decode/stream passes.
    /// Defaults to `1` (fully serial) so an unqualified run never saturates the machine; pass a
    /// higher number to parallelize, or `0` to use rayon's default = all logical CPUs.
    #[arg(long, default_value_t = 1)]
    pub threads: usize,

    /// Also emit whole-way linestrings into a dedicated `way_geometries` table (in addition to the
    /// always-emitted intersection-split `geometries` table). Needed to materialize relation
    /// geometries without re-merging split segments at query time.
    #[arg(long, default_value_t = false)]
    pub emit_way_geometries: bool,

    /// Emit one point row per classified node into a `node_geometries` table.
    #[arg(long, default_value_t = false)]
    pub emit_node_geometries: bool,

    /// Emit one merged-linestring row per kept relation into a `relation_geometries` table, built
    /// (as a post-processing SQL step) by merging each relation's member-way geometries — reusing
    /// `way_geometries` if `--emit-way-geometries` is also set, otherwise merging the split segments
    /// on the fly. Postgres output only.
    #[arg(long, default_value_t = false)]
    pub emit_relation_geometries: bool,

    /// Create indexes on the tag/geom tables after loading. Off by default: indexing a large
    /// import (especially the split geom table's GiST) can dominate runtime, so it's opt-in for
    /// when the tables are actually queried.
    #[arg(long, default_value_t = false)]
    pub create_index: bool,

    /// Parallel COPY connections per table. Rows are round-robined across them, so the dominant
    /// table (e.g. roads) isn't bottlenecked on a single connection's serialization + ingest.
    /// Uses up to `(topics + 1) × db_writers` Postgres connections during load. Ignored for `csv`.
    #[arg(long, default_value_t = 4)]
    pub db_writers: usize,

    /// Output backend: `pg` (COPY into PostGIS) or `csv` (one file per tag table + geometries.csv).
    #[arg(long, value_enum, default_value_t = Output::Pg)]
    pub output: Output,

    /// Tile-server output joining each topic's tag table to `geometries` on `osm_id`, exposed as
    /// `<topic>_tiles`: `none` (default), `view` (cheap, always fresh), or `materialized` (physical
    /// table + GiST spatial index, what a tile server renders from). Ignored for `csv`.
    #[arg(long, value_enum, default_value_t = Tiles::None)]
    pub tiles: Tiles,

    /// Directory for CSV output (created if missing). Only used with `--output csv`.
    #[arg(long, default_value = "out")]
    pub out_dir: String,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Output {
    Pg,
    Csv,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, clap::ValueEnum)]
#[value(rename_all = "kebab-case")]
pub enum Tiles {
    /// Don't create any tile relation. Default.
    None,
    /// Create a `<topic>_tiles` view per topic.
    View,
    /// Materialize a `<topic>_tiles` table per topic + GiST spatial index.
    Materialized,
}
