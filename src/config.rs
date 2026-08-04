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
    /// `--source csv`, which reads ways from stdin instead (see `Source`).
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
    /// `geojson` (same CSV files, plus one `<table>.geojson` `FeatureCollection` per topic), or
    /// `geojsonseq` (same CSV files, plus one `<table>.geojsonseq` newline-delimited GeoJSON
    /// Feature stream per topic — RFC 8142). Both GeoJSON variants are built by joining tag rows to
    /// edge geometries on `osm_id`, for local tooling like the live editor; `geojsonseq` streams
    /// without buffering the whole topic, `geojson` is simpler to consume whole.
    #[arg(long, value_enum, default_value_t = Output::Pg)]
    pub output: Output,

    /// Directory for CSV/GeoJSON(Seq) output (created if missing). Only used with `--output
    /// csv`/`geojson`/`geojsonseq`.
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

    /// Where to read elements from: `pbf` decodes `pbf_file` as usual; `csv` instead classifies ways
    /// by tags alone, read as CSV from stdin — no PBF decode, no geometry at all (only valid with
    /// `--output csv`; there's nothing to build a `pg`/`geojsonseq` output from). Built for the live
    /// editor: whichever caller (e.g. `editor/lib/liveEditor.ts`) is responsible for selecting which
    /// ways to run (bbox query, way-id search, ...) and streams them in as `osm_id,tags_json` rows —
    /// see `csv_source`'s own doc for the exact schema, and for why geometry never crosses this
    /// boundary.
    #[arg(long, value_enum, default_value_t = Source::Pbf)]
    pub source: Source,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Source {
    Pbf,
    Csv,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Output {
    Pg,
    Csv,
    #[value(name = "geojson")]
    GeoJson,
    #[value(name = "geojsonseq")]
    GeoJsonSeq,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum TopicEdgeMode {
    /// Just the routing columns: id/seg/start/end/geom/length/cost/reverse_cost.
    Pgrouting,
    /// The above, plus the topic's own tag columns (`produced`/`annotations`/`meta`) joined in.
    All,
}
