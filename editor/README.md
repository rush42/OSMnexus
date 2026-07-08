# Live editor

A local web app for iterating on topic/category JSON against a real map: pick a bbox, edit a
category's condition (or a topic's transforms/fields) in a JSON editor, save, and the pipeline
reruns and the map re-renders — no manual `cargo run` / reload round-trips.

It's a thin wrapper around the same [`osmnexus`](../README.md) binary: saving an edit shells out
to the release build with `--config-dir <the selected configs/* dir> --output geojson`, then the
map reloads the resulting `<topic>.geojson` files.

The editor operates directly on the repo's real config directories under `../configs/` — there's
no private copy to keep in sync. A **config selector** (top of the topics panel) lists every
directory under `configs/` (`tilda`, `osmnx`, `public_transport`, ...) and lets you switch between
them; whichever one is selected is what saves write to and what the pipeline runs against. Edits
made here are real edits to the repo's configs, not a sandboxed copy.

## How it works

- **`vite.config.ts`** — a Vite plugin (`liveEditorApi`) that serves a small JSON API alongside the
  dev server: listing/switching configs, extracting a bbox (via `osmium extract`), listing
  topics/categories, reading/writing a category or topic's JSON file, and re-running the pipeline
  after every write.
- **`src/App.tsx`** / **`Map.tsx`** / **`Editor.tsx`** — the React UI: a MapLibre map for bbox
  selection and rendering classified features, and a CodeMirror JSON editor for the selected
  topic/category file.
- **`fixtures/tiny.osm.pbf`** — the default base extract when no larger one is mounted (see
  `BASE_PBF_PATH` below).

## Running it

The editor always runs in Docker now — no Node.js or `osmium-tool` install needed, and no local
`node_modules` to keep in sync.

`docker-compose.yml` runs it in a container (installs `osmium-tool` + npm deps, mounts the repo so
edits persist on the host). It still shells out to a host-built `target/release/osmnexus` (mounted
in along with the rest of the repo), so `cargo build --release` from the repo root is still needed
once before `docker compose up` (and again after changing Rust code):

```bash
cd editor
docker compose up
```

Open http://localhost:5173, draw a bbox on the map (bounded by `MAX_BBOX_M`, default 3000m), and
it extracts that area, runs the pipeline, and renders the result. Edit a category/topic JSON and
save to re-run and re-render.

Environment variables:

| var | default | meaning |
|---|---|---|
| `BASE_PBF_PATH` | `fixtures/tiny.osm.pbf` | the base `.osm.pbf` bbox selections are extracted from |
| `MAX_BBOX_M` | `3000` | max side length (meters) of a selectable bbox |

To use a bigger base extract (e.g. `berlin.osm.pbf` at the repo root), point `BASE_PBF` at it
before `docker compose up`.

### Trying it standalone (no repo checkout)

The root [`Dockerfile`](../Dockerfile) builds a fully self-contained image — release pipeline
binary, editor, and a freshly-downloaded Berlin extract baked in — for handing to someone who just
wants to try the tool without cloning the repo or building anything themselves:

```bash
docker build -t tilda-live-editor-demo .
docker run --rm -p 5173:5173 tilda-live-editor-demo
```

This one doesn't mount the repo, so edits made in it don't persist anywhere — use
`docker compose up` above for actual config-editing work.

## Editing configs

Topics and categories are plain JSON files under `<config>/<topic>/` (e.g.
`../configs/tilda/bikelanes/`), where `<config>` is whichever directory is picked in the config
selector:

- `topic.json` — table name, transforms, osm fields, sanitizers, deriver bindings.
- `way/*.json`, `node/*.json`, `relation/*.json` — one file per category.

The editor's save button writes the edited JSON straight to these files (validating it parses
first) and triggers a pipeline rerun — the same files you'd hand-edit outside the editor. See the
[root README](../README.md#topics-are-data) for the full config format.
