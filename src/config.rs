use std::path::PathBuf;

use clap::Parser;

/// Default max branch depth of the `categorize` discrimination net (`decision_tree`).
/// The single source of truth for this default — also read by `decision_tree` as the fallback for
/// callers (e.g. tests) that build a tree without going through CLI parsing.
/// Tried raising this to 10 for `bikelanes/way`'s benefit (smaller worst-case/avg leaf size: 14→12,
/// 5.07→3.59) but measured ~30% *slower* end-to-end on a real Brandenburg run (Pass A 18.0s→24.8s,
/// total 26.8s→35.0s, cache-warm-controlled) — building the larger tree (554→970 nodes) apparently
/// costs more than the smaller leaves save at this data volume. Reverted; leaf-size metrics don't
/// predict real throughput here, so don't raise this without benchmarking end-to-end again.
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

    /// Output backend: `pg` (COPY into PostGIS), `csv` (one text file per tag/geometry table), or
    /// `geojson`/`geojsonseq`/`parquet`, which stage their tag/geometry tables as `.bin` files (the
    /// pipeline's own row-struct staging format — see `output::stage`'s own doc) instead of text
    /// CSV, then join them into one `<table>.geojson` `FeatureCollection` per topic /
    /// one `<table>.geojsonseq` newline-delimited GeoJSON Feature stream per topic (RFC 8142) / one
    /// `<table>.parquet` GeoParquet file per topic, forward-joining tag rows to geometry rows on
    /// `osm_id` without a hashmap (see `output::cursor`'s own doc) — for local tooling like the live
    /// editor, or downstream analytical tools that read Parquet/GeoParquet directly.
    #[arg(long, value_enum, default_value_t = Output::Pg)]
    pub output: Output,

    /// Directory for CSV/GeoJSON(Seq)/Parquet output (created if missing). Only used with file
    /// outputs.
    #[arg(long, default_value = "out")]
    pub out_dir: String,

    /// Decimal places to round output coordinates to.
    ///
    /// The PBF stores coordinates as fixed-point *decimicrodegrees* (1e-7 degrees), so **7 is the
    /// source data's exact resolution** — nothing in the input is more precise than that. The
    /// pipeline projects to Web Mercator internally and unprojects on the way out, and the inverse
    /// projection's `atan`/`exp` leaves latitudes carrying round-trip noise far past that: a stored
    /// `53.1016340` comes back out as `53.101634000000004`. (Longitude survives — its transform is
    /// linear.) So `--coordinate-precision 7` *recovers* the stored value rather than discarding
    /// information, and drops roughly a third of the coordinate bytes in GeoJSON output, which is
    /// dominated by them.
    ///
    /// The default is deliberately above anything an `f64` can express (its shortest round-trip
    /// form is at most 17 significant digits), so by default this rounds nothing at all and output
    /// stays byte-identical to previous releases. Rounding is opt-in.
    ///
    /// Applies to every backend that writes coordinates, not just GeoJSON — a run emits one
    /// coordinate precision, so `.parquet` WKB and `.geojson` agree.
    #[arg(long, default_value_t = 21)]
    pub coordinate_precision: u32,

    /// Max branch depth of the `categorize` discrimination net (see `decision_tree`).
    /// Deeper trees prune more aggressively at the cost of build time; shallower trees fall back to
    /// larger leaves sooner. `0` skips compiling a tree at all and classifies by walking
    /// `categories.json`'s `order` linearly instead (see `categorize_linear` in
    /// `categorize::categories`) — also useful as a debugging/perf comparison against the
    /// tree-based classifier.
    #[arg(long, default_value_t = DEFAULT_TREE_MAX_DEPTH)]
    pub tree_max_depth: usize,

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
    Parquet,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum TopicEdgeMode {
    /// Just the routing columns: id/seg/start/end/geom/length/cost/reverse_cost.
    Pgrouting,
    /// The above, plus the topic's own tag columns (`produced`/`annotations`/`meta`) joined in.
    All,
}

/// How many decimal places output coordinates are rounded to — see `Config::coordinate_precision`
/// for why 7 is the meaningful value and why the default rounds nothing.
#[derive(Clone, Copy, Debug)]
pub struct CoordPrecision(pub u32);

impl CoordPrecision {
    /// At or above this, rounding is skipped entirely. Not just an optimization: an `f64`'s
    /// shortest round-trip form is at most 17 significant digits, so a larger precision cannot
    /// change any value — but computing it anyway would, since `v * 1e21` overflows past the range
    /// where `round()` means anything and dividing back does not recover `v`. The guard is what
    /// makes a large default a true no-op rather than silent corruption.
    const PASSTHROUGH: u32 = 17;

    pub fn round(self, v: f64) -> f64 {
        if self.0 >= Self::PASSTHROUGH {
            return v;
        }
        let factor = 10f64.powi(self.0 as i32);
        (v * factor).round() / factor
    }
}

#[cfg(test)]
mod coord_precision_tests {
    use super::CoordPrecision;

    #[test]
    fn rounds_away_the_unprojection_noise_to_the_stored_value() {
        // What `mercator_to_wgs84` hands back for a PBF-stored 53.1016340.
        assert_eq!(CoordPrecision(7).round(53.101634000000004), 53.101634);
        assert_eq!(CoordPrecision(7).round(53.10165960000001), 53.1016596);
        // Longitude is already clean; rounding must leave it alone.
        assert_eq!(CoordPrecision(7).round(8.8805731), 8.8805731);
    }

    #[test]
    fn the_default_precision_changes_nothing() {
        // The guard, not the arithmetic, is what makes this hold: `v * 1e21` would not survive the
        // round trip.
        for v in [53.101634000000004, 8.8805731, -0.0001, 179.9999999999, 0.0] {
            assert_eq!(CoordPrecision(21).round(v), v, "default precision altered {v}");
        }
    }

    #[test]
    fn negative_coordinates_round_symmetrically() {
        assert_eq!(CoordPrecision(7).round(-53.101634000000004), -53.101634);
    }
}
