# Changelog

Notable changes to this repo, kept for future-me/future-agent context. Newest first.

## Unreleased

- Live editor (`editor/`) generalized from a single hardcoded fixture to arbitrary areas:
  - `editor/docker-compose.yml` now mounts a configurable base `.osm.pbf` via `BASE_PBF` (defaults to `../berlin.osm.pbf`) at `/data/base.osm.pbf`, passed to the dev server as `BASE_PBF_PATH`.
  - `editor/Dockerfile` installs `osmium-tool`.
  - `editor/vite.config.ts`: added `POST /api/extract` which runs `osmium extract -s complete_ways` against the base PBF for a user-picked bbox and switches the live editor to that extract; `GET /api/bounds` now returns the base file's full extent (via `osmium fileinfo -e -j`) until an extract is picked, then the extract's bounds plus `selected: true`.
  - Frontend (`editor/src/Map.tsx`, `editor/src/App.tsx`): shift+drag on the map draws a bbox and calls `/api/extract`; a banner prompts selection until one is made.
  - Fixed a latent bug found while wiring this up: the editor was invoking a stale `osm-pipeline` binary and reading a `geometries.csv` file that no longer exist — the current pipeline binary is `osmnexus` and it writes `edges.csv` (see `src/output/rows.rs` `EDGE_TABLE`/`GEOM_COLUMNS`). Updated `PIPELINE_BIN` and the CSV filename read in `buildFeatureCollection` accordingly.
