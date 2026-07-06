# OSMnexus — configurable OSM network extraction

A streaming OSM → PostGIS **network-extraction** engine. It reads an `.osm.pbf` extract, classifies
elements into topics using **data-defined JSON rules**, and writes a normalized graph to
PostgreSQL/PostGIS: per-topic **tag** tables plus shared **geometry** tables, joined on `osm_id`.

It is a configurable alternative to tools like [OSMnx](https://github.com/gboeing/osmnx): the engine hardcodes *no*
particular network. What counts as an edge, how tags are normalized, and which attributes are
derived all live in JSON config, so you can carve **any** kind of network out of OSM — streets,
bike infrastructure, transit, footpaths, a power grid — by writing rules, not code.

Two bundled configs show that range: [`configs/tilda`](configs/tilda) reimplements the
`roads_bikelanes` topic from [tilda-geo](https://github.com/FixMyBerlin/tilda-geo/) (the
radverkehrsatlas processing pipeline), producing the `roads` and `bikelanes` classifications;
[`configs/osmnx`](configs/osmnx) reimplements an [OSMnx](https://github.com/gboeing/osmnx)-style
driveable-streets network. Nothing in the engine is specific to roads; point `--config-dir` at
your own folder to define a different network.

## How it works

A topic classifies **all three OSM primitives** — ways, nodes, and relations — each independently,
using its own set of categories:

- **Ways** are selected by matching their tags against the topic's `way/*.json` categories. No
  separate element filter: a way is in the topic iff some category matches (after the topic's
  `exclude_condition` and shared tag transforms). Categories are required to be pairwise
  **disjoint** (checked at load time via their `excludes` declarations) — at most one category can
  ever match a given object, so there's no first-match ambiguity to resolve at runtime. That
  disjointness invariant is what lets categorization be compiled once, ahead of time, into a
  **decision tree** (branching on discriminating tags like `highway`) that each element is
  evaluated against — turning classification into a handful of tag lookups instead of scanning
  every category.
- **Relations** work the other way round: a relation matches a `relation/*.json` category by its
  own tags, and when it does, *all of its member ways are pulled into the graph* — even if a
  member way's own tags wouldn't otherwise match. The relation contributes its own tag row (via
  `relation_members`, linking `relation_id` ↔ member `way_id`), the geometry still comes from the
  member ways.
- **Nodes** are classified the same way (`node/*.json` categories, e.g. barriers, crossings,
  traffic signals) but don't add graph edges — a classified node instead forces a **graph split**:
  the way carrying it gets cut at that node even if it's otherwise only used by one way (normally a
  cut point requires the node to be shared by ≥2 ways, i.e. an intersection).

Once selected, an element is enriched the same way regardless of kind: **transforms** (e.g.
side-splitting a way into `left`/`right` cycleway objects) run first, then **categorization**
picks one category, then **sanitizers** (per-field value cleanup) and **derivers** (rule-based
derived attributes, e.g. inferred `surface`) fill in the output columns. Categories can override
which deriver feeds a given output column.

## Data model

One extracted graph; each topic is a disjoint attribute layer over it.

- **`<topic>`** — one tag row per (element, side, prefix). Tag-only classification, no geometry.
  Columns: `osm_id, osm_type, id, osm, derived, private, meta` (`derived.minzoom` is an ordinary
  derived field). `id` = `way/{id}` (or
  `way/{id}/{prefix}/left|right` for side objects, `node/{id}`, `relation/{id}`). An element can
  appear in several topics with *different* classifications, so tag tables are per-topic.
- **`edges`** — one row per (way, segment), shared across all topics: the graph's edges, cut at
  intersections and at any classified node. Columns: `osm_id, seg_idx, start_id, end_id,
  geom(LineString,3857), length_m, total_length_m`.
- **`way_geometries`** (opt-in, `--emit-way-geometries`) — one whole-way linestring per way,
  uncut. Needed to materialize relation geometries without re-merging split segments.
- **`node_geometries`** (opt-in, `--emit-node-geometries`) — one point row per classified node.
- **`relation_geometries`** (opt-in, `--emit-relation-geometries`, Postgres output only) — one
  merged linestring per kept relation, built as a post-load SQL step from its member ways'
  geometries (reusing `way_geometries` if present, otherwise merging split segments on the fly).
- **`relation_members`** — link table, `relation_id` ↔ member `way_id`, for joining relation rows
  back to their constituent ways' geometry.

Materialize tiles/features by joining on `osm_id`:

```sql
SELECT r.*, g.geom, g.length_m
FROM roads r JOIN edges g USING (osm_id);
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
cargo run --release -- brandenburg-latest.osm.pbf --create-index
```

The tables (`roads`, `bikelanes`, `edges`, `relation_members`) are created and truncated
automatically.

### Example

A typical PostGIS run that also materializes whole-way and relation geometries and builds indexes
afterwards:

```bash
cargo run --release -- brandenburg-latest.osm.pbf \
    --emit-way-geometries \      # materialize uncut whole-way linestrings
    --emit-relation-geometries \ # merge member-way geometries per relation (needs the above)
    --create-index \             # build the GiST/btree indexes after load
    --db-writers 8                # parallel COPY connections per table
```

Prefer files over a database? Point it at the `csv` backend — same schema, no Postgres needed:

```bash
cargo run --release -- brandenburg-latest.osm.pbf \
    --output csv \          # write CSV instead of COPY-ing into PostGIS
    --out-dir ./out          # roads.csv, bikelanes.csv, edges.csv, ...
```

Add `RUST_LOG=info` in front of either command for per-phase timings.

## Configuration

| flag | default | meaning |
|---|---|---|
| `<pbf_file>` (or `PBF_FILE`) | — | input `.osm.pbf` |
| `--config-dir <path>` | `configs/tilda` | topic config folder to run (e.g. [`configs/osmnx`](configs/osmnx)) |
| `--output <backend>` | `pg` | `pg` (COPY into PostGIS) or `csv` (one file per tag table + geometry tables) |
| `--out-dir <path>` | `out` | directory for CSV output (`--output csv` only) |
| `--emit-way-geometries` | off | also emit uncut whole-way linestrings (`way_geometries`) |
| `--emit-node-geometries` | off | also emit one point row per classified node (`node_geometries`) |
| `--emit-relation-geometries` | off | also emit one merged linestring per kept relation (`relation_geometries`, `pg` only) |
| `--create-index` | off | build indexes after load (the split-geom GiST can dominate runtime; `pg` only) |
| `--db-writers <k>` | `4` | parallel COPY connections per table, rows round-robined (`pg` only) |
| `--truncate` | on | truncate tables before loading (`pg` only) |
| `--threads <n>` | `1` | rayon pool size for the CPU-bound passes (`0` = all cores) |
| `--left-hand-traffic` | off | flip which physical side a side-split object's `forward`/`backward` tags are read from |

The `csv` backend writes the same schema as `pg` — `<topic>.csv` (tag tables), `edges.csv`,
and any opted-in geometry tables — with a header row, JSON columns as quoted CSV, and geometry as
hex-encoded EWKB. Load it anywhere (DuckDB, `ogr2ogr`, `COPY … FROM`, pandas, a graph library via
the `osm_id`/`start_id`/`end_id` edge list).

Database connection uses the standard libpq env vars, overridable per flag: `PGHOST`/`--db-host`
(empty → Unix socket, peer auth), `PGDATABASE`/`--db-name`, `PGUSER`/`--db-user`,
`PGPASSWORD`/`--db-password`, `PGPORT`/`--db-port`.

Profiling: set `PASS_C_PROFILE=1` for a per-stage CPU breakdown; `RUST_LOG=info` for phase timings.

## Topics are data

Each topic lives under `<config-dir>/<name>/` and is pure data — no Rust changes to add or edit
one:

- `topic.json` — table name, transforms, osm fields, sanitizers, deriver bindings, exclude
  condition.
- `way/*.json`, `node/*.json`, `relation/*.json` — per-kind category conditions, required to be
  pairwise disjoint via `excludes`, compiled into a decision tree.
- `derivers.json`, `macros.json` — derived outputs and reusable filter fragments.

Shared macros/sanitizers/value-sets live in `<config-dir>/_shared/`.

## Layout

```
src/          engine (classify, transforms, geometry, DB writers), reader, main
configs/      data-defined config directories (tilda, example, ...), each with its own topics
BACKLOG.md    deferred ideas / performance notes
```

## Attribution & license

Derived from [tilda-geo](https://github.com/tordans/tilda-geo). Licensed under the **GNU AGPL v3** —
see [LICENSE.md](LICENSE.md).
