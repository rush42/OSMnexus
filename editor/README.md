# Live editor

A local web app for iterating on topic/category JSON against a real map: pick a bbox, edit a
category's condition (or a topic's transforms/fields) in a JSON editor, save, and the pipeline
reruns and the map re-renders — no manual `cargo run` / reload round-trips.

It's a thin wrapper around the same [`osmnexus`](../README.md) binary: saving an edit shells out
to the release build with `--config-dir editor/live-config --output geojson`, then the map reloads
the resulting `<topic>.geojson` files.

## How it works

- **`live-config/`** — the config directory the editor runs against (its own `bike`/`drive`/`walk`
  topics + `_shared/`), separate from `configs/` so editing here never touches the real configs.
- **`vite.config.ts`** — a Vite plugin (`liveEditorApi`) that serves a small JSON API alongside the
  dev server: extracting a bbox (via `osmium extract`), listing topics/categories, reading/writing
  a category or topic's JSON file, and re-running the pipeline after every write.
- **`src/App.tsx`** / **`Map.tsx`** / **`Editor.tsx`** — the React UI: a MapLibre map for bbox
  selection and rendering classified features, and a CodeMirror JSON editor for the selected
  topic/category file.
- **`fixtures/tiny.osm.pbf`** — the default base extract when no larger one is mounted (see
  `BASE_PBF_PATH` below).

## Running it

Prerequisites: a release build of the pipeline (`cargo build --release` from the repo root — the
editor shells out to `target/release/osmnexus`), Node.js, and `osmium-tool` (for bbox extraction)
on `PATH`.

```bash
cd editor
npm install
npm run dev
```

Open http://localhost:5173, draw a bbox on the map (bounded by `MAX_BBOX_M`, default 3000m), and
it extracts that area, runs the pipeline, and renders the result. Edit a category/topic JSON and
save to re-run and re-render.

### Docker

`docker-compose.yml` runs the same thing in a container (installs `osmium-tool` + npm deps, mounts
the repo so edits persist on the host):

```bash
docker compose up
```

Environment variables (also settable directly for `npm run dev`):

| var | default | meaning |
|---|---|---|
| `BASE_PBF_PATH` | `fixtures/tiny.osm.pbf` | the base `.osm.pbf` bbox selections are extracted from |
| `MAX_BBOX_M` | `3000` | max side length (meters) of a selectable bbox |

To use a bigger base extract (e.g. `berlin.osm.pbf` at the repo root), point `BASE_PBF` at it
before `docker compose up`, or set `BASE_PBF_PATH` directly for `npm run dev`.

## Editing configs

Topics and categories are plain JSON files under `live-config/<topic>/`:

- `topic.json` — table name, transforms, osm fields, sanitizers, deriver bindings.
- `way/*.json`, `node/*.json`, `relation/*.json` — one file per category.

The editor's save button writes the edited JSON straight to these files (validating it parses
first) and triggers a pipeline rerun — the same files you'd hand-edit outside the editor. See the
[root README](../README.md#topics-are-data) for the full config format.
