use std::path::PathBuf;

use clap::Parser;

/// Default max branch depth of the `categorize` discrimination net (`decision_tree`).
/// The single source of truth for this default — also read by `decision_tree` as the fallback for
/// callers (e.g. tests) that build a tree without going through CLI parsing.
pub const DEFAULT_TREE_MAX_DEPTH: usize = 6;

#[derive(Parser, Debug)]
#[command(about = "Data-driven OSM PBF topic processing pipeline → PostgreSQL")]
pub struct Config {
    /// Path to the .osm.pbf file. Required for `--source pbf` (the default); ignored/omittable for
    /// `--source postgis`, which reads elements from the database instead (see `Source`).
    #[arg(env = "PBF_FILE", default_value = "")]
    pub pbf_file: String,

    /// Config directory: a self-contained folder of topic dirs plus a shared config-root library
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

    /// The import region drives on the left (UK, Japan, Australia, ...). Flips which physical side
    /// (`left`/`right`) a side-split object's `forward`/`backward` directed tags are read from.
    /// Right-hand traffic (the OSM/global default) is assumed unless set.
    #[arg(long, default_value_t = false)]
    pub left_hand_traffic: bool,

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

    /// Output backend: `pg` (COPY into PostGIS), `csv` (one file per tag table + geometries.csv),
    /// or `geojsonseq` (same CSV files, plus one `<table>.geojsonseq` newline-delimited GeoJSON
    /// Feature stream per topic — RFC 8142 — built by joining tag rows to edge geometries on
    /// `osm_id`; for local tooling like the live editor).
    #[arg(long, value_enum, default_value_t = Output::Pg)]
    pub output: Output,

    /// Directory for CSV/GeoJSONSeq output (created if missing). Only used with `--output csv`/`geojsonseq`.
    #[arg(long, default_value = "out")]
    pub out_dir: String,

    /// Max branch depth of the `categorize` discrimination net (see `decision_tree`).
    /// Deeper trees prune more aggressively at the cost of build time; shallower trees fall back to
    /// larger leaves sooner.
    #[arg(long, default_value_t = DEFAULT_TREE_MAX_DEPTH)]
    pub tree_max_depth: usize,

    /// Bypass the compiled decision tree and classify by walking `categories.json`'s `order`
    /// linearly (see `categorize_linear` in `categorize::categories`) — for debugging/perf
    /// comparison against the tree-based classifier.
    #[arg(long, default_value_t = false)]
    pub linear_classify: bool,

    /// Shape of the per-topic `{table}_edge` pgRouting table, for topics that declare
    /// `"geometry": { "way": ["graph"] }` in their `topic.json` (see
    /// `TopicRunner::wants_way_graph`) — built as a post-processing SQL step after the shared edge
    /// table is loaded (and indexed, if `--create-index` is set). `pgrouting` emits only the
    /// routing columns; `all` additionally joins in the topic's own tag columns
    /// (`produced`/`annotations`/`meta`). Postgres output only; ignored by topics that don't
    /// declare `graph`.
    #[arg(long, value_enum, default_value_t = TopicEdgeMode::Pgrouting)]
    pub topic_edges: TopicEdgeMode,

    /// Where to read elements from: `pbf` decodes `pbf_file` as usual; `postgis` instead reads
    /// already-loaded ways (tags + geometry) from `source_table`/`source_table`_geom in the
    /// database addressed by the `db_*` fields, filtered to `bbox` — no PBF decode, no node-coord
    /// resolution (the geometry is already known). Built for the live editor: a one-time "all ways"
    /// pass loads a whole region into Postgres (see `configs/live_raw/topic.json`), then every bbox
    /// edit re-runs only the tiny per-topic tag/filter/producer pipeline over that bbox's rows.
    #[arg(long, value_enum, default_value_t = Source::Pbf)]
    pub source: Source,

    /// Bbox filter for `--source postgis`, as `min_lon,min_lat,max_lon,max_lat` (WGS84). Required
    /// when `--source postgis` is set; ignored otherwise.
    #[arg(long)]
    pub bbox: Option<String>,

    /// Table holding the "all ways" pass's raw tag rows for `--source postgis` (its geometry
    /// sibling is `{source_table}_geom`, per `schema::way_geom_table`) — the output `table` of
    /// whatever topic.json was used for that pass, e.g. `live_raw`.
    #[arg(long, default_value = "live_raw")]
    pub source_table: String,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Source {
    Pbf,
    Postgis,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Output {
    Pg,
    Csv,
    #[value(name = "geojsonseq")]
    GeoJson,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum TopicEdgeMode {
    /// Just the routing columns: id/seg/start/end/geom/length/cost/reverse_cost.
    Pgrouting,
    /// The above, plus the topic's own tag columns (`produced`/`annotations`/`meta`) joined in.
    All,
}
