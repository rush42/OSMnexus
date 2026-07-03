# tilda-geo — OSM topic engine (Rust)

A streaming OSM → PostGIS **network-extraction** tool. It reads an `.osm.pbf` extract, classifies
ways into topics using data-defined JSON rules, and writes a normalized graph to PostgreSQL/PostGIS:
per-topic **tag** tables plus one shared **geometry** table, joined on `osm_id`.

This is a Rust reimplementation of the `roads_bikelanes` topic from
[tilda-geo](https://github.com/tordans/tilda-geo) (the radverkehrsatlas processing pipeline). It
produces the `roads` and `bikelanes` classifications; the other Lua topics (parking, transit, POIs,
traffic signs, …) are not ported.

## Data model

One extracted graph; each topic is a disjoint attribute layer over it.

- **`<topic>`** — one tag row per (way, side, prefix). Tag-only classification, no geometry.
  Columns: `osm_id, osm_type, id, osm, derived, private, meta, minzoom`. `id` = `way/{id}` (or
  `way/{id}/{prefix}/left|right` for side objects). A way can appear in several topics with
  *different* classifications, so tag tables are per-topic.
- **`geometries`** — one row per (way, variant, segment), shared across all topics. Columns:
  `osm_id, variant, seg_idx, start_id, end_id, geom(LineString,3857), length_m, total_length_m`.
  `variant` is `way` (whole way) or `split` (one row per intersection sub-linestring); `start_id` /
  `end_id` are the OSM node ids at each end (graph-topology seeds). A way's geometry is
  topic-independent, so it's stored once.

Materialize tiles/features by joining on `osm_id`:

```sql
SELECT r.*, g.geom, g.length_m
FROM roads r JOIN geometries g USING (osm_id)
WHERE g.variant = 'way';
```

Classification is **tag-only** — no geometry-derived criteria (length, etc.). Length/graph-based
filtering belongs to a downstream geometry/graph stage, not to tag classification.

## Quick start

Prerequisites: a recent Rust toolchain and a reachable **PostGIS-enabled Postgres** (the tool only
writes; it doesn't provision a database).

```bash
# 1. Get an extract (any Geofabrik .osm.pbf)
curl -O https://download.geofabrik.de/europe/germany/brandenburg-latest.osm.pbf

# 2. Point at your database (libpq env vars) and run
export PGHOST=/var/run/postgresql PGDATABASE=geo PGUSER=me
cargo run --release -- brandenburg-latest.osm.pbf --split both --create-index
```

The tables (`roads`, `bikelanes`, `geometries`) are created and truncated automatically.

## Configuration

| flag | default | meaning |
|---|---|---|
| `<pbf_file>` (or `PBF_FILE`) | — | input `.osm.pbf` |
| `--split <mode>` | `ways` | geometry variant(s): `ways`, `intersections`, or `both` |
| `--create-index` | off | build indexes after load (the split-geom GiST can dominate runtime) |
| `--db-writers <k>` | `4` | parallel COPY connections per table (rows round-robined) |
| `--truncate` | on | truncate tables before loading |
| `--threads <n>` | `0` | rayon pool size for the CPU-bound passes (`0` = all cores) |

Database connection uses the standard libpq env vars, overridable per flag: `PGHOST`/`--db-host`
(empty → Unix socket, peer auth), `PGDATABASE`/`--db-name`, `PGUSER`/`--db-user`,
`PGPASSWORD`/`--db-password`, `PGPORT`/`--db-port`.

Profiling: set `PASS_C_PROFILE=1` for a per-stage CPU breakdown; `RUST_LOG=info` for phase timings.

## How it works

A single decode of the PBF split into three streaming passes:

1. **Pass A** — decode the way region once: filter by each topic's `element_filter`, tally per-node
   use-counts (→ intersection nodes), classify from tags, and emit tag rows.
2. **Pass B** — decode the node region once for the referenced coordinates.
3. **Geometry pass** — resolve each kept way's line + cut-points and emit geometry rows (no second
   decode). Rows stream to sharded per-table COPY writers concurrently with the passes.

## Topics are data

Each topic lives under `topics/<name>/` and is pure data — no Rust changes to add or edit one:

- `topic.json` — table name, `element_filter`, transforms, osm fields, deriver bindings.
- `categories/*.json` — category conditions (compiled into a first-match priority order via
  `excludes`), `macros.json`.
- `sanitizers.json`, `derivers.json` — value normalization and derived outputs.

Shared macros/sanitizers/value-sets live in `topics/_shared/`.

## Layout

```
src/          engine (classify, transforms, geometry, DB writers), reader, main
topics/       data-defined topic definitions (bikelanes, roads, _shared)
BACKLOG.md    deferred ideas / performance notes
```

## Attribution & license

Derived from [tilda-geo](https://github.com/tordans/tilda-geo). Licensed under the **GNU AGPL v3** —
see [LICENSE.md](LICENSE.md).
