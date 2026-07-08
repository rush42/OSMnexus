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
binary is compiled into the image, no host Rust toolchain needed. To iterate on Rust code without a
full image rebuild each time, point at a host-built binary instead:
`PIPELINE_BIN_PATH=/repo/target/release/osmnexus docker compose up` (after `cargo build --release`
from the repo root).

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

## How it works

It's a thin wrapper around the same [`osmnexus`](../README.md) binary: saving an edit shells out
to the release build with `--config-dir <the selected configs/* dir> --output geojson`, then the
map reloads the resulting `<topic>.geojson` files.

A **config selector** (top of the topics panel) lists every directory under `configs/` (`tilda`,
`osmnx`, `public_transport`, ...) and lets you switch between them; whichever one is selected is
what saves write to and what the pipeline runs against — a private copy, not the repo's actual
files.

- **`vite.config.ts`** — a Vite plugin (`liveEditorApi`) that serves a small JSON API alongside the
  dev server: listing/switching configs, extracting a bbox (via `osmium extract`), listing
  topics/categories, reading/writing a category or topic's JSON file, and re-running the pipeline
  after every write.
- **`src/App.tsx`** / **`Map.tsx`** / **`Editor.tsx`** — the React UI: a MapLibre map for bbox
  selection and rendering classified features, and a CodeMirror JSON editor for the selected
  topic/category file.
- **`fixtures/tiny.osm.pbf`** — the default base extract when no larger one is mounted (see
  `BASE_PBF_PATH` above).

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
