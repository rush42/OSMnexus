use clap::{Parser, ValueEnum};

/// How the geometry table is populated. Independent of the (tag-only) classification.
#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "kebab-case")]
pub enum SplitMode {
    /// One whole-way linestring per way (`variant='way'`). Default.
    Ways,
    /// One sub-linestring per intersection segment (`variant='split'`).
    Intersections,
    /// Emit both the whole-way row and the intersection segments.
    Both,
}

#[derive(Parser, Debug)]
#[command(about = "Data-driven OSM PBF topic processing pipeline → PostgreSQL")]
pub struct Config {
    /// Path to the .osm.pbf file
    #[arg(env = "PBF_FILE")]
    pub pbf_file: String,

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
    /// `0` (default) uses rayon's default = the number of logical CPUs. Set to `1` for a fully
    /// serial run, or a lower number to leave cores free for the rest of the system.
    #[arg(long, default_value_t = 0)]
    pub threads: usize,

    /// Geometry table variant(s) to emit: `ways` (whole ways, default), `intersections`
    /// (one row per intersection segment), or `both`.
    #[arg(long, value_enum, default_value_t = SplitMode::Ways)]
    pub split: SplitMode,

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
