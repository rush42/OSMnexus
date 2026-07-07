# Changelog

Notable changes to this repo, kept for future-me/future-agent context. Newest first.

## Unreleased

- Live editor visualizes routing-graph cut points: `editor/vite.config.ts` `buildFeatureCollection` now keeps *all* edge segments per `osm_id` (it used to keep only the last one, silently dropping segments of ways split at intersections) and emits a second `cutPoints` FeatureCollection — the shared node between consecutive segments of the same way, i.e. where the graph broke a way apart. Rendered as small orange dots (`editor/src/Map.tsx`, `CUT_POINTS_SOURCE_ID`), fed from `editor/src/App.tsx`'s new `cutPoints` state.
- Live editor now starts on a bundled `example` topic (`editor/live-config/example/`, matching `highway=cycleway`) instead of hardcoded `bikelanes_simple/way/bikeway`. The right-hand panel is minimizable (▶/◀ toggle) and lists all categories under the topic (`GET /api/categories/:topic`, scans `way/`/`node`/`relation` dirs), with a text input + "+" to add a new category (writes a blank `{"condition":{}}` file via the existing classify endpoint and reruns the pipeline).
- Live editor (`editor/`) generalized from a single hardcoded fixture to arbitrary areas:
  - `editor/docker-compose.yml` now mounts a configurable base `.osm.pbf` via `BASE_PBF` (defaults to `../berlin.osm.pbf`) at `/data/base.osm.pbf`, passed to the dev server as `BASE_PBF_PATH`.
  - `editor/Dockerfile` installs `osmium-tool`.
  - `editor/vite.config.ts`: added `POST /api/extract` which runs `osmium extract -s complete_ways` against the base PBF for a user-picked bbox and switches the live editor to that extract; `GET /api/bounds` now returns the base file's full extent (via `osmium fileinfo -e -j`) until an extract is picked, then the extract's bounds plus `selected: true`.
  - Frontend (`editor/src/Map.tsx`, `editor/src/App.tsx`): shift+drag on the map draws a bbox and calls `/api/extract`; a banner prompts selection until one is made.
  - Fixed a latent bug found while wiring this up: the editor was invoking a stale `osm-pipeline` binary and reading a `geometries.csv` file that no longer exist — the current pipeline binary is `osmnexus` and it writes `edges.csv` (see `src/output/rows.rs` `EDGE_TABLE`/`GEOM_COLUMNS`). Updated `PIPELINE_BIN` and the CSV filename read in `buildFeatureCollection` accordingly.
