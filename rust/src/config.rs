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
    /// Uses up to `2 × topics × db_writers` Postgres connections during load.
    #[arg(long, default_value_t = 4)]
    pub db_writers: usize,
}
