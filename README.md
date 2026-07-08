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

There's also a [live editor](editor/) — a local web app for iterating on category/topic JSON
against a real map and seeing the classified output re-render on every save.

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
  geom(LineString,3857), length_m, total_length_m, cost, reverse_cost`. `start_id`/`end_id` join
  `nodes.id`; `cost`/`reverse_cost` (pgRouting-style) always equal `length_m` here — this shared
  table stays topic-neutral. A topic that wants real routing weights defines `cost`/`is_directed`
  fields and opts into `--topic-edges` (below) for a `{topic}_edge` table with those baked in.
- **`nodes`** — always emitted, one row per graph vertex: every node referenced as a `start_id`/
  `end_id` in `edges` (shared by ≥2 ways, a way endpoint, or forced by a node classifier). Columns:
  `id, osm_id, geom(Point,3857)`. `id` is the internal sequential vertex id; `osm_id` is the
  original OSM node id, kept for lookups/debugging.
- **`way_geometries`** (opt-in, `--emit-way-geometries`) — one whole-way linestring per way,
  uncut. Needed to materialize relation geometries without re-merging split segments.
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

### Per-topic routing tables (`--topic-edges`)

A topic can define two extra fields to describe its own graph semantics, using the same
field/filter machinery as everything else — no bespoke expression language:

- **`cost`** — a numeric field (an `osm_fields`/`sanitizers` entry, or a topic/category `consts`
  value), e.g. `{ "tag": "cost", "name": "parse_length", "in": ["width"] }`.
- **`is_directed`** — a boolean field driven by a `Filter` condition (the `FilterMatch` producer),
  e.g. `{ "output": "is_directed", "source": { "filter": { "tag": "oneway", "in": ["yes", "-1"] } } }`.

Pass `--topic-edges <pgrouting|all>` and, as a post-load SQL step, any topic that defines `cost`
gets a `{topic}_edge` table: `cost`/`reverse_cost` (pgRouting convention — `-1` means unusable in
that direction) computed from the topic's `cost`/`is_directed` fields and split proportionally by
segment share (`length_m / total_length_m`) of the shared `edges` table. `all` additionally joins
in the topic's own tag columns (`osm`/`derived`/`private`/`meta`); `pgrouting` emits only the
routing columns. Indexes on `{topic}_edge` respect `--create-index` like every other table.

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

The tables (`roads`, `bikelanes`, `edges`, `nodes`, `relation_members`) are created and truncated
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
    --out-dir ./out          # roads.csv, bikelanes.csv, edges.csv, nodes.csv, ...
```

Add `RUST_LOG=info` in front of either command for per-phase timings.

## Configuration

| flag | default | meaning |
|---|---|---|
| `<pbf_file>` (or `PBF_FILE`) | — | input `.osm.pbf` |
| `--config-dir <path>` | `configs/tilda` | topic config folder to run (e.g. [`configs/osmnx`](configs/osmnx)) |
| `--output <backend>` | `pg` | `pg` (COPY into PostGIS), `csv` (one file per tag table + geometry tables), or `geojson` (same CSVs, plus one `<topic>.geojson` per topic) |
| `--out-dir <path>` | `out` | directory for CSV/GeoJSON output (`--output csv`/`geojson` only) |
| `--emit-way-geometries` | off | also emit uncut whole-way linestrings (`way_geometries`) |
| `--emit-relation-geometries` | off | also emit one merged linestring per kept relation (`relation_geometries`, `pg` only) |
| `--create-index` | off | build indexes after load (the split-geom GiST can dominate runtime; `pg` only) |
| `--topic-edges <mode>` | off | materialize `{topic}_edge` routing tables for topics that define `cost`; `pgrouting` (routing columns only) or `all` (+ joined tag columns) — see [Per-topic routing tables](#per-topic-routing-tables---topic-edges) (`pg` only) |
| `--db-writers <k>` | `4` | parallel COPY connections per table, rows round-robined (`pg` only) |
| `--truncate` | on | truncate tables before loading (`pg` only) |
| `--threads <n>` | `1` | rayon pool size for the CPU-bound passes (`0` = all cores) |
| `--left-hand-traffic` | off | flip which physical side a side-split object's `forward`/`backward` tags are read from |
| `--tree-max-depth <n>` | `6` | max branch depth of the categorization decision tree; deeper prunes more aggressively at the cost of build time |

The `csv`/`geojson` backends write the same schema as `pg` — `<topic>.csv` (tag tables),
`edges.csv`, and any opted-in geometry tables — with a header row, JSON columns as quoted CSV, and
geometry as hex-encoded EWKB. Load it anywhere (DuckDB, `ogr2ogr`, `COPY … FROM`, pandas, a graph
library via the `osm_id`/`start_id`/`end_id` edge list). `geojson` additionally joins each topic's
tag rows to edge geometries (by `osm_id`) and reprojects to WGS84, writing one
`<topic>.geojson` `FeatureCollection` per topic — used by the [live editor](editor/).

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

## Live editor

[`editor/`](editor/) is a local web app for iterating on topic/category JSON against a real map:
pick a bbox, edit a category's condition or a topic's transforms/fields in a JSON editor, save, and
the pipeline reruns (`--output geojson`) and the map re-renders — no manual CLI round-trips. See
[`editor/README.md`](editor/README.md) for setup and usage.

## Layout

```
src/          engine (classify, transforms, geometry, DB writers), reader, main
configs/      data-defined config directories (tilda, osmnx, ...), each with its own topics
editor/       live editor — local web app for iterating on topic/category JSON against a map
BACKLOG.md    deferred ideas / performance notes
```

## Attribution & license

Derived from [tilda-geo](https://github.com/tordans/tilda-geo). Licensed under the **GNU AGPL v3** —
see [LICENSE.md](LICENSE.md).
