# Live editor

A local web app for iterating on topic/category JSON against a real map: pick a bbox, edit a
category's condition (or a topic's transforms/fields) in a JSON editor, save, and the pipeline
reruns and the map re-renders — no manual `cargo run` / reload round-trips.

Edits made in the editor are sandboxed — they never touch the repo's real `configs/*` files, and
don't persist past the session.

## Running it

Always run this through Docker — no local Node.js, `osmium-tool`, or `npm install` needed.

```bash
cd editor
docker compose up
```

`docker compose up` builds the image automatically the first time (rebuild explicitly with
`docker compose up --build` after changing Rust code, `Dockerfile`, or `package.json`) — the pipeline
binary (and the `dag_json` tree-view binary) is compiled into the image, no host Rust toolchain
needed. To iterate on Rust code without a full image rebuild each time, point at host-built binaries
instead: `PIPELINE_BIN_PATH=/repo/target/release/osmnexus DAG_JSON_BIN_PATH=/repo/target/release/dag_json
docker compose up` (after `cargo build --release --bin osmnexus --bin dag_json` from the repo root).

This also starts a `db` (PostGIS) container. Before the editor has anything to show, load a
region's ways into it once (see `docker-compose.yml`'s comment on the `db` service for the exact
command) — the editor's bbox selection queries that table, it doesn't parse the PBF itself.

Open http://localhost:5173, draw a bbox on the map (bounded by `MAX_BBOX_M`, default 10000m), and
it queries that bbox from Postgres, runs the pipeline, and renders the result. Edit a category/topic
JSON and save to re-run and re-render.

Environment variables:

| var | default | meaning |
|---|---|---|
| `BASE_PBF_PATH` | `fixtures/tiny.osm.pbf` | the base `.osm.pbf` the one-time "all ways" ingest pass reads |
| `MAX_BBOX_M` | `10000` | max side length (meters) of a selectable bbox |
| `LIVE_SOURCE_TABLE` | `live_raw` | the ingest pass's output table (tags in this table, geometry in `<table>_geom`) |
| `PGHOST`/`PGPORT`/`PGDATABASE`/`PGUSER`/`PGPASSWORD` | see `docker-compose.yml` | connection to the `db` service |

To use a bigger base extract (e.g. `berlin.osm.pbf` at the repo root), point `BASE_PBF` at it
before `docker compose up` (used only by the one-time ingest pass, not read per bbox drag).

### Trying it standalone (no repo checkout)

The root [`Dockerfile`](../Dockerfile) predates the Postgres-backed source (it bundles a single
all-in-one container with no `db` service) and is currently stale — it needs a Postgres/PostGIS
service and the ingest pass wired in before it'll work again. Use `editor/docker-compose.yml`
until that's updated.

## How it works

It's a thin wrapper around the same [`osmnexus`](../README.md) binary: saving an edit shells out
to the release build with `--source postgis --bbox <the selected bbox> --config-dir <the selected
configs/* dir> --output geojsonseq`, then the map reloads the resulting `<topic>.geojsonseq` files.

A **config selector** (top of the topics panel) lists every directory under `configs/` (`tilda`,
`osmnx`, `public_transport`, ...) and lets you switch between them; whichever one is selected is
what saves write to and what the pipeline runs against — a private copy, not the repo's actual
files.

It's a Next.js (App Router) app — a real Next.js server, not the old Vite-dev-server-plugin hack:

- **`app/api/**/route.ts`** — one route handler per endpoint (listing/switching configs, recording
  a bbox selection, listing topics/categories, reading/writing a category or topic's JSON file,
  re-running the pipeline after every write, and plotting a topic's output `Producer` trees via
  `dag_json`). Business logic lives in **`lib/liveEditor.ts`**, which every route handler imports;
  its module state (current bbox/config) is pinned to `globalThis` because Next.js compiles each
  route file into its own module graph — see that file's own comment for why a plain module-level
  variable silently doesn't work here.
- **`components/App.tsx`** / **`Map.tsx`** / **`Editor.tsx`** / **`DagView.tsx`** — the React UI
  (all client components — `"use client"`): a MapLibre map for bbox selection and rendering
  classified features, a CodeMirror JSON editor for the selected topic/category file (the "Text"
  tab), and an `@xyflow/react` graph view (the "Tree" tab) that plots the selected topic's
  output-field `Producer` trees — each field's `Match`/`Extract`/`Const`/... tree, and the
  `Sanitizer` chain hanging off any `Extract` leaf, same walk as `src/bin/plot_dag.rs`'s Graphviz
  DOT output but as JSON (`src/dag.rs`) rendered in-browser.
- **`app/layout.tsx`** / **`app/page.tsx`** — the root shell; `page.tsx` just renders `App`.
- **`fixtures/tiny.osm.pbf`** — the default base extract when no larger one is mounted (see
  `BASE_PBF_PATH` above).

Feature source moved off `osmium extract` per bbox drag onto Postgres/PostGIS: a one-time "all
ways" pass loads a whole region's ways (tags + geometry) into a `live_raw`/`live_raw_geom` table
(`configs/live_raw/topic.json`), and every bbox selection is now a spatial query against it
(`src/live_source.rs`, `--source postgis`) instead of a PBF re-parse. See the root `docker-compose.yml`'s
`db` service and its comment for the one-off ingest command.

## Editing configs

Topics and categories are plain JSON files under `<config>/<topic>/` (e.g.
`../configs/tilda/bikelanes/`), where `<config>` is whichever directory is picked in the config
selector:

- `topic.json` — table name, transforms, osm fields, sanitizers, deriver bindings.
- `way/*.json`, `node/*.json`, `relation/*.json` — one file per category.

The editor's save button validates the edited JSON and triggers a pipeline rerun, same file layout
as hand-editing outside the editor. Copy the result back into the real `configs/` file yourself
once you're happy with it. See the [root README](../README.md#topics-are-data) for the full config
format.
