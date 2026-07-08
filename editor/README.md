# Live editor

A local web app for iterating on topic/category JSON against a real map: pick a bbox, edit a
category's condition (or a topic's transforms/fields) in a JSON editor, save, and the pipeline
reruns and the map re-renders — no manual `cargo run` / reload round-trips.

It's a thin wrapper around the same [`osmnexus`](../README.md) binary: saving an edit shells out
to the release build with `--config-dir <the selected configs/* dir> --output geojson`, then the
map reloads the resulting `<topic>.geojson` files.

The editor never touches the repo's real config directories under `../configs/`. A **config
selector** (top of the topics panel) lists every directory under `configs/` (`tilda`, `osmnx`,
`public_transport`, ...) and lets you switch between them; on first use (or when you switch), the
server copies that config into a scratch temp directory and every read/write/delete from then on
happens against the copy, which the pipeline also runs against. Edits made in the editor are
sandboxed — they never land in the repo's actual configs, and disappear once the dev server (or
container) restarts.

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
the pipeline binary/source stay in sync with the host — but see above, config edits themselves
still go to a scratch copy, not the mounted `configs/`). It still shells out to a host-built
`target/release/osmnexus`, so `cargo build --release` from the repo root is still needed once
before `docker compose up` (and again after changing Rust code):

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

This one doesn't mount the repo at all — configs are baked in at build time and, same as above,
edits go to a scratch copy inside the container, so nothing persists once the container stops.

## Editing configs

Topics and categories are plain JSON files under `<config>/<topic>/` (e.g.
`../configs/tilda/bikelanes/`), where `<config>` is whichever directory is picked in the config
selector:

- `topic.json` — table name, transforms, osm fields, sanitizers, deriver bindings.
- `way/*.json`, `node/*.json`, `relation/*.json` — one file per category.

The editor's save button validates the edited JSON, writes it to the same-named file in the
scratch copy described above, and triggers a pipeline rerun — the same file layout you'd hand-edit
outside the editor, just not the same files on disk (see above: nothing here persists past the
session). Use the editor to iterate on a condition/rule and then copy the result back into the
real `configs/` file yourself once you're happy with it. See the
[root README](../README.md#topics-are-data) for the full config format.
