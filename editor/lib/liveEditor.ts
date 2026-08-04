// Server-side logic for the live editor, ported from the old Vite-plugin-as-backend
// (`vite.config.ts`'s `liveEditorApi`) into a plain module that Next.js route handlers
// (`app/api/**/route.ts`) import — Next.js gives every API endpoint its own route file, so the
// dozen-odd `if (url.pathname === ...)` branches that used to live in one middleware function are
// gone; what's left here is the shared state (current bbox/config) and the actual work each route
// does. Runs only on the server (route handlers are Node.js-runtime by default) — safe to use
// `node:child_process`/`node:fs` here.

import { spawn } from "node:child_process";
import { promises as fs } from "node:fs";
import path from "node:path";
import os from "node:os";

import { csvLine, parseCsv } from "./csv";
import { linestringFromEwkb } from "./ewkb";
import { parseCopyBinary } from "./pgBinaryCopy";

const EDITOR_DIR = path.resolve(process.cwd());
const REPO_DIR = path.resolve(EDITOR_DIR, "..");
// Defaults to the image's baked-in build (see editor/Dockerfile); override to point at a
// host-built target/release/osmnexus instead, for iterating on Rust code without a full image
// rebuild (e.g. `PIPELINE_BIN_PATH=/repo/target/release/osmnexus docker compose up`).
const PIPELINE_BIN = process.env.PIPELINE_BIN_PATH || path.join(REPO_DIR, "target", "release", "osmnexus");
// Emits a topic's output Producer trees as node/edge JSON for the tree view — see `src/bin/dag_json.rs`.
const DAG_JSON_BIN = process.env.DAG_JSON_BIN_PATH || path.join(REPO_DIR, "target", "release", "dag_json");
const CONFIGS_ROOT = path.join(REPO_DIR, "configs");
// The table an "all ways" pass loaded a whole region into (tags in `SOURCE_TABLE`, geometry in
// `SOURCE_TABLE`_geom — see `configs/live_raw/topic.json` and `fetchWays` below). A bbox
// selection is a spatial query against this table, not an `osmium extract` + full PBF reparse.
const SOURCE_TABLE = process.env.LIVE_SOURCE_TABLE || "live_raw";
// DB connection for the bounds/way-select queries below (`psqlQuery`/`fetchWays`) — the
// pipeline subprocess itself no longer talks to Postgres (see `src/csv_source.rs`).
const PG_ENV = {
  PGHOST: process.env.PGHOST || "",
  PGDATABASE: process.env.PGDATABASE || "postgres",
  PGUSER: process.env.PGUSER || "postgres",
  PGPASSWORD: process.env.PGPASSWORD || "",
  PGPORT: process.env.PGPORT || "5432",
};
// Upper bound on a shift-dragged bbox's side length (meters), configurable via docker-compose so a
// deployment with a bigger base region (and tolerance for slower queries) can raise it.
export const MAX_BBOX_M = Number(process.env.MAX_BBOX_M) || 10000;

/** Thrown by any of this module's functions to signal an HTTP status a route handler should use. */
export class ApiError extends Error {
  status: number;
  constructor(status: number, message: string) {
    super(message);
    this.status = status;
  }
}

// Mutable server state, deliberately pinned to `globalThis` rather than plain module-level `let`s.
// Next.js compiles each `route.ts` into its own module graph (even in dev, with on-demand/Fast
// Refresh recompilation splitting things further) — a plain `let` here ends up as a *separate*
// instance per route file, so e.g. `/api/config`'s write and `/api/topics`'s read silently touch
// different variables. `globalThis` is the one thing guaranteed to be the same object across all
// of them within one Node.js process (the same trick Next.js docs recommend for a singleton
// Prisma client, for the same underlying reason).
//
// - `currentBounds`: the bbox currently in use for the map/pipeline. Starts out unset until the
//   user picks one.
// - `currentWayId`: set instead of (alongside, for map display) `currentBounds` by a way-id search
//   (see `selectWay`) — when set, `fetchWays` queries `t.osm_id = currentWayId` rather than the
//   bbox spatial query, so the pipeline runs on exactly the searched-for way and nothing else
//   nearby. Cleared by a manual bbox selection.
// - `currentConfigName`: which `configs/*` directory is selected — just a name, no copy (see
//   `listTopicsForConfig`, which reads the real tree directly; there's nothing left that needs a
//   full multi-topic scratch copy now that the pipeline only ever runs against one topic at a
//   time — see `currentTopicDir` below).
// - `currentTopicName`/`currentTopicDir`: the one topic currently being edited/run against.
//   `currentTopicDir` is a scratch copy under the OS temp dir containing ONLY that topic's
//   subdirectory plus `currentConfigName`'s shared root-level files (`macros.json`,
//   `sanitizers.json`, etc. — see `switchTopic`) — never the real configs/* tree, and never the
//   other topics in that config. This is what every file read/write and pipeline/dag_json
//   invocation resolves against. Edits made in the live editor are therefore discarded when the
//   dev server restarts and never touch the repo's actual configs/*.
const globalState = globalThis as unknown as {
  __liveEditor?: {
    currentBounds: [number, number, number, number] | null;
    currentWayId: string | null;
    currentConfigName: string | null;
    currentTopicName: string | null;
    currentTopicDir: string | null;
  };
};
const state = (globalState.__liveEditor ??= {
  currentBounds: null,
  currentWayId: null,
  currentConfigName: null,
  currentTopicName: null,
  currentTopicDir: null,
});

// A config is any non-`_`-prefixed directory directly under CONFIGS_ROOT (configs/tilda,
// configs/osmnx, configs/public_transport, ...) — same discovery rule as listTopicsForConfig()
// below, one level up.
export async function listConfigs(): Promise<string[]> {
  const entries = await fs.readdir(CONFIGS_ROOT, { withFileTypes: true });
  return entries
    .filter((e) => e.isDirectory() && !e.name.startsWith("_"))
    .map((e) => e.name)
    .sort();
}

// A topic is any non-`_`-prefixed directory directly under a config dir — same discovery rule the
// Rust side uses (see TopicRunner::load_all). Reads the real `configs/<config>/` tree directly
// (read-only, no copy) — this only ever needs to enumerate names for the topic dropdown/
// `switchTopic` validation, never to serve file content, so there's nothing to isolate a scratch
// copy for here.
export async function listTopicsForConfig(config: string): Promise<string[]> {
  const entries = await fs.readdir(path.join(CONFIGS_ROOT, config), { withFileTypes: true });
  return entries
    .filter((e) => e.isDirectory() && !e.name.startsWith("_"))
    .map((e) => e.name)
    .sort();
}

// Topics of whichever config is currently selected — see `listTopicsForConfig`.
export async function listTopics(): Promise<string[]> {
  return listTopicsForConfig(requireConfig());
}

function requireConfig(): string {
  if (!state.currentConfigName) {
    throw new ApiError(409, "no config selected — pick one from the start page first");
  }
  return state.currentConfigName;
}

function requireTopicDir(): string {
  if (!state.currentTopicDir) {
    throw new ApiError(409, "no topic selected — pick a config and topic first");
  }
  return state.currentTopicDir;
}

// Confirms `topic` is the currently active one (a cheap state-equality check — replaces the old
// "does this directory exist under the config copy" scan now that exactly one topic is ever in
// play) and returns the scratch dir it lives under.
function requireCurrentTopic(topic: string): string {
  const dir = requireTopicDir();
  if (topic !== state.currentTopicName) {
    throw new ApiError(400, `topic '${topic}' is not the currently selected topic ('${state.currentTopicName}')`);
  }
  return dir;
}

// Peak RSS (kB) of a running process, read from the kernel-maintained "high water mark" in
// `/proc/<pid>/status` — already the max over the process's lifetime, so a single successful read
// at any point while it's alive is enough; no need to poll-and-max ourselves. Linux-only (matches
// the Docker image this runs in); returns `null` on any other platform or read failure.
async function peakRssKb(pid: number): Promise<number | null> {
  try {
    const status = await fs.readFile(`/proc/${pid}/status`, "utf-8");
    const match = status.match(/^VmHWM:\s*(\d+) kB$/m);
    return match ? Number(match[1]) : null;
  } catch {
    return null;
  }
}

// `stdin`, when given, is written to the child's stdin and the stream is closed — used to hand
// the pipeline binary a CSV way stream (see `fetchWays`/`runPipeline`) without a temp file.
// `trackMemory` polls the child's peak RSS (see `peakRssKb`) while it runs — off by default since
// most callers (`psql`, etc.) don't care and the poll loop is needless overhead for them.
function run(
  bin: string,
  args: string[],
  env?: Record<string, string>,
  stdin?: string,
  options?: { trackMemory?: boolean },
): Promise<{ ok: true; stdout: string; peakRssKb: number | null } | { ok: false; message: string }> {
  return new Promise((resolve) => {
    const child = spawn(bin, args, {
      stdio: [stdin != null ? "pipe" : "ignore", "pipe", "pipe"],
      env: env ? { ...process.env, ...env } : process.env,
    });
    let stdout = "";
    let stderr = "";
    let peakRss: number | null = null;
    const memInterval =
      options?.trackMemory && child.pid
        ? setInterval(async () => {
            const kb = await peakRssKb(child.pid!);
            if (kb != null) peakRss = kb;
          }, 25)
        : null;
    // Non-null: `stdio`'s indices 1/2 are always `"pipe"` above regardless of `stdin`, and index 0
    // is `"pipe"` exactly when `stdin != null` below — TS can't narrow a ternary in an object
    // literal to a specific `spawn` overload, so it falls back to the nullable general type.
    child.stdout!.on("data", (d) => (stdout += d.toString()));
    child.stderr!.on("data", (d) => (stderr += d.toString()));
    child.on("error", (err) => {
      if (memInterval) clearInterval(memInterval);
      resolve({ ok: false, message: String(err) });
    });
    child.on("close", async (code) => {
      // One last read: VmHWM is a monotonic high-water mark, so this catches a final growth spurt
      // the last poll tick missed, right up until the process actually exits.
      if (options?.trackMemory && child.pid) {
        const kb = await peakRssKb(child.pid);
        if (kb != null) peakRss = kb;
      }
      if (memInterval) clearInterval(memInterval);
      if (code === 0) resolve({ ok: true, stdout, peakRssKb: peakRss });
      else resolve({ ok: false, message: stderr || `${bin} exited with code ${code}` });
    });
    if (stdin != null) {
      child.stdin!.write(stdin);
      child.stdin!.end();
    }
  });
}

// One `-tAc` (tuples-only, unaligned) query against the base region database via `psql`, using the
// same `PG*` connection env the pipeline binary reads (see `PG_ENV`/`src/db/pool.rs`).
async function psqlQuery(sql: string): Promise<string> {
  const args = ["-tAc", sql];
  if (PG_ENV.PGHOST) args.unshift("-h", PG_ENV.PGHOST, "-p", PG_ENV.PGPORT, "-U", PG_ENV.PGUSER, "-d", PG_ENV.PGDATABASE);
  else args.unshift("-U", PG_ENV.PGUSER, "-d", PG_ENV.PGDATABASE);
  const result = await run("psql", args, PG_ENV);
  if (!result.ok) throw new Error(result.message);
  return result.stdout.trim();
}

// Like `psqlQuery`, but for a `COPY ... TO STDOUT (FORMAT binary)` statement: collects stdout as a
// raw `Buffer` instead of decoding it as UTF-8 text — `psqlQuery`'s `.toString()` would corrupt
// binary column data (EWKB doubles, etc.), since arbitrary bytes aren't valid UTF-8. `-t`/`-A`
// aren't needed here (they only affect how a normal `SELECT` resultset prints; a COPY stream
// bypasses that formatting entirely regardless), so this skips them and passes `-c` directly.
function psqlCopyBinary(sql: string): Promise<Buffer> {
  const args = ["-c", sql];
  if (PG_ENV.PGHOST) args.unshift("-h", PG_ENV.PGHOST, "-p", PG_ENV.PGPORT, "-U", PG_ENV.PGUSER, "-d", PG_ENV.PGDATABASE);
  else args.unshift("-U", PG_ENV.PGUSER, "-d", PG_ENV.PGDATABASE);
  return new Promise((resolve, reject) => {
    const child = spawn("psql", args, { stdio: ["ignore", "pipe", "pipe"], env: { ...process.env, ...PG_ENV } });
    const chunks: Buffer[] = [];
    let stderr = "";
    child.stdout!.on("data", (d: Buffer) => chunks.push(d));
    child.stderr!.on("data", (d) => (stderr += d.toString()));
    child.on("error", (err) => reject(err));
    child.on("close", (code) => {
      if (code === 0) resolve(Buffer.concat(chunks));
      else reject(new Error(stderr || `psql exited with code ${code}`));
    });
  });
}

// The bbox (WGS84) covering every way the "all ways" pass loaded — used only to center the map on
// first load, before the user has drawn a bbox of their own.
async function baseTableBounds(): Promise<[number, number, number, number]> {
  const row = await psqlQuery(
    `SELECT ST_XMin(e)||','||ST_YMin(e)||','||ST_XMax(e)||','||ST_YMax(e) ` +
      `FROM (SELECT ST_Extent(ST_Transform(geom, 4326)) e FROM ${SOURCE_TABLE}_geom) s`,
  );
  const bbox = row.split(",").map(Number);
  if (bbox.length !== 4 || bbox.some((n) => Number.isNaN(n))) {
    throw new Error(`could not read bounds from ${SOURCE_TABLE}_geom — did you run the "all ways" ingest pass?`);
  }
  return bbox as [number, number, number, number];
}

// The bbox, tags, and WGS84 geometry of a single way, looked up by osm_id — the bbox query reuses
// the `{table}_osm_id_idx` btree index (see `src/db/schema.rs`), same cost class as
// `baseTableBounds`'s full-extent query. `osmId` is validated as an integer before being
// interpolated, since `psqlQuery` has no parameterized-query path (see `baseTableBounds` above,
// which does the same). Tags/geometry are fetched separately (rather than packed into one
// comma-joined row like `baseTableBounds`) since a GeoJSON geometry can itself contain commas;
// `chr(30)` (ASCII unit separator) is used as the field delimiter there instead, since it can't
// appear in `produced`'s or `ST_AsGeoJSON`'s text output.
export async function findWay(osmId: string): Promise<{ bbox: [number, number, number, number]; geometry: unknown; tags: Record<string, unknown> }> {
  if (!/^-?\d+$/.test(osmId)) throw new ApiError(400, "way id must be an integer");
  const bboxRow = await psqlQuery(
    `SELECT ST_XMin(e)||','||ST_YMin(e)||','||ST_XMax(e)||','||ST_YMax(e) ` +
      `FROM (SELECT ST_Extent(ST_Transform(geom, 4326)) e FROM ${SOURCE_TABLE}_geom WHERE osm_id = ${osmId}) s`,
  );
  const bbox = bboxRow.split(",").map(Number);
  if (bbox.length !== 4 || bbox.some((n) => Number.isNaN(n))) {
    throw new ApiError(404, `way ${osmId} not found in ${SOURCE_TABLE}_geom`);
  }
  const detailRow = await psqlQuery(
    `SELECT t.produced::text || chr(30) || ST_AsGeoJSON(ST_Transform(g.geom, 4326)) ` +
      `FROM ${SOURCE_TABLE} t JOIN ${SOURCE_TABLE}_geom g ON g.osm_id = t.osm_id WHERE t.osm_id = ${osmId} LIMIT 1`,
  );
  const sep = detailRow.indexOf("\x1e");
  if (sep === -1) throw new ApiError(500, `could not read tags/geometry for way ${osmId}`);
  let tags: Record<string, unknown> = {};
  try {
    tags = JSON.parse(detailRow.slice(0, sep));
  } catch {
    // Malformed/absent `produced` JSON shouldn't block showing the geometry — falls back to {}.
  }
  const geometry = JSON.parse(detailRow.slice(sep + 1));
  return { bbox: bbox as [number, number, number, number], geometry, tags };
}

// Selects the ways the pipeline should run on (bbox spatial query, or a single `t.osm_id =` lookup
// for a way-id search) and returns two things pulled from one query: `tagsCsv` — `osm_id,tags_json`
// per way, the exact schema `src/csv_source.rs` parses off the pipeline's stdin — and `geometry`, a
// `osm_id -> WGS84 GeoJSON geometry` map this module keeps to itself. The pipeline never sees
// geometry at all for this source (see `src/csv_source.rs`'s own doc for why): `--source csv`
// classifies tags only, so joining the classified rows back to a way's shape is this module's job,
// done in `buildFeatureCollections` after the pipeline returns.
//
// Fetched via `FORMAT binary` rather than `FORMAT csv` + `ST_AsGeoJSON` (an earlier version of
// this function): a CSV/GeoJSON round trip means comma/quote-escaping every `tags_json` field and,
// worse, printing every coordinate as full ASCII decimal text — measured as the dominant cost once
// the pipeline itself stopped handling geometry (see `src/csv_source.rs`'s doc for that change).
// Binary COPY sends `produced` as raw UTF-8 bytes (no CSV escaping) and geometry as raw EWKB (16
// bytes/point, `ST_AsEWKB`) instead of GeoJSON's ~40+ ASCII bytes/point — `parseCopyBinary` unwraps
// the wire format, `linestringFromEwkb` decodes the geometry, mirroring `src/geom/primitives.rs`.
async function fetchWays(
  target: { wayId: string } | { bounds: [number, number, number, number] },
): Promise<{ tagsCsv: string; geometry: Map<string, GeoJSON.Geometry> }> {
  const selectCols = "t.osm_id, t.produced::text, ST_AsEWKB(g.geom)";
  const from = `${SOURCE_TABLE} t JOIN ${SOURCE_TABLE}_geom g ON g.osm_id = t.osm_id`;
  let where: string;
  if ("wayId" in target) {
    if (!/^-?\d+$/.test(target.wayId)) throw new ApiError(400, "way id must be an integer");
    where = `t.osm_id = ${target.wayId}`;
  } else {
    const [minLon, minLat, maxLon, maxLat] = target.bounds;
    where = `ST_Intersects(g.geom, ST_Transform(ST_MakeEnvelope(${minLon}, ${minLat}, ${maxLon}, ${maxLat}, 4326), 3857))`;
  }
  const buf = await psqlCopyBinary(`COPY (SELECT ${selectCols} FROM ${from} WHERE ${where}) TO STDOUT (FORMAT binary)`);
  const geometry = new Map<string, GeoJSON.Geometry>();
  let tagsCsv = "";
  for (const [osmIdField, tagsField, geomField] of parseCopyBinary(buf)) {
    const osmId = osmIdField!.readBigInt64BE(0).toString();
    const tagsJson = tagsField!.toString("utf8");
    tagsCsv += csvLine([osmId, tagsJson]);
    if (geomField) {
      geometry.set(osmId, { type: "LineString", coordinates: linestringFromEwkb(geomField) });
    }
  }
  return { tagsCsv, geometry };
}

// Looks a way up, selects it (see `selectWay`), and runs the pipeline against exactly that way —
// the single request the way-search UI needs. Returns the merged multi-topic FeatureCollection
// plus the way's own bbox/geometry/tags, so the frontend can both display whatever the pipeline
// classified it as *and*, if the pipeline didn't classify it into anything, fall back to rendering
// its raw geometry/tags directly (see `App.tsx`'s `searchWay`).
export async function searchAndRunWay(osmId: string): Promise<unknown> {
  const way = await findWay(osmId);
  selectWay(osmId, way.bbox);
  const pipelineResult = (await runPipelineAndRespond()) as Record<string, unknown>;
  return { ...pipelineResult, bounds: way.bbox, wayGeometry: way.geometry, wayTags: way.tags };
}

export async function getBounds(): Promise<{ bounds: [number, number, number, number]; selected: boolean; maxBboxM: number }> {
  if (state.currentBounds) return { bounds: state.currentBounds, selected: true, maxBboxM: MAX_BBOX_M };
  try {
    const bounds = await baseTableBounds();
    return { bounds, selected: false, maxBboxM: MAX_BBOX_M };
  } catch (err) {
    throw new ApiError(500, String(err));
  }
}

// Selecting a bbox used to shell out to `osmium extract` (cut a fresh PBF slice, then re-parse it
// from scratch on every single edit). Now it's just recording the bounds — the pipeline queries
// `SOURCE_TABLE`/`SOURCE_TABLE`_geom for exactly this bbox on every run instead (see `runPipeline`).
// Clears `currentWayId` — a fresh manual bbox drag supersedes any earlier way-id search.
export function selectBbox(bounds: [number, number, number, number]): { bounds: [number, number, number, number] } {
  state.currentBounds = bounds;
  state.currentWayId = null;
  return { bounds: state.currentBounds };
}

// A way-id search: records both the way's own osm_id (which the pipeline filters on directly, see
// `runPipeline`) and a bbox around it (bounds are still useful to the frontend for `fitBounds` even
// though the pipeline itself ignores them whenever `currentWayId` is set).
export function selectWay(osmId: string, bounds: [number, number, number, number]): { bounds: [number, number, number, number] } {
  state.currentBounds = bounds;
  state.currentWayId = osmId;
  return { bounds: state.currentBounds };
}

async function runPipeline(
  target: { wayId: string } | { bounds: [number, number, number, number] },
  outDir: string,
): Promise<
  { ok: true; geometry: Map<string, GeoJSON.Geometry>; peakRssKb: number | null } | { ok: false; message: string }
> {
  const configDir = requireTopicDir();
  const { tagsCsv, geometry } = await fetchWays(target);
  const result = await run(
    PIPELINE_BIN,
    ["--source", "csv", "--config-dir", configDir, "--output", "csv", "--out-dir", outDir, "--linear-classify"],
    undefined,
    tagsCsv,
    { trackMemory: true },
  );
  return result.ok ? { ok: true, geometry, peakRssKb: result.peakRssKb } : result;
}

// Deletes a scratch topic dir some time after it stops being `currentTopicDir` — not immediately.
// An in-flight pipeline/dag_json subprocess call captures `currentTopicDir`'s value up front (see
// `runPipeline`/`getDag`) and keeps reading files out of it for the life of that one request; if a
// second `switchTopic`/`switchConfig` call landed in the meantime (e.g. two topic-select requests
// firing close together) and deleted that same directory immediately, the in-flight request could
// have a category file vanish out from under it mid-read — surfacing as a confusing "exclude
// references unknown category" error with no actual config problem. A delay long enough to outlast
// any real request (pipeline runs here are consistently well under a second) avoids that without
// needing to track in-flight request counts.
const SCRATCH_DIR_CLEANUP_DELAY_MS = 30_000;
function scheduleCleanup(dir: string | null): void {
  if (!dir) return;
  setTimeout(() => {
    fs.rm(dir, { recursive: true, force: true }).catch(() => {});
  }, SCRATCH_DIR_CLEANUP_DELAY_MS);
}

// Selecting a config no longer implies picking (or copying) a topic — that's `switchTopic`'s job,
// called separately once the topic dropdown has something to select.
export async function switchConfig(config: string): Promise<void> {
  if (!(await listConfigs()).includes(config)) {
    throw new ApiError(400, `unknown config '${config}'`);
  }
  state.currentConfigName = config;
  state.currentTopicName = null;
  scheduleCleanup(state.currentTopicDir);
  state.currentTopicDir = null;
}

// Builds a scratch dir containing ONLY `topic`'s subtree plus the current config's shared
// root-level files (`macros.json`/`producers.json`/`sanitizers.json`/`units.json`/
// `value_sets.json` — whichever actually exist; matched by "is a file, not a directory" rather
// than a hardcoded name list, since every topic is a directory and every shared file lives flat
// at the config root, per `TopicRunner::load`'s own file layout). This is the "run this topic as
// if it were its own config" piece: `--config-dir` ends up pointing at a dir containing exactly
// one topic subdirectory, so the pipeline/dag_json classify only that topic, not its siblings.
export async function switchTopic(topic: string): Promise<void> {
  const config = requireConfig();
  const topics = await listTopicsForConfig(config);
  if (!topics.includes(topic)) {
    throw new ApiError(400, `unknown topic '${topic}' in config '${config}'`);
  }
  const configSrc = path.join(CONFIGS_ROOT, config);
  const workDir = await fs.mkdtemp(path.join(os.tmpdir(), "live-editor-topic-"));

  const rootEntries = await fs.readdir(configSrc, { withFileTypes: true });
  await Promise.all(
    rootEntries
      .filter((e) => !e.isDirectory())
      .map((e) => fs.copyFile(path.join(configSrc, e.name), path.join(workDir, e.name))),
  );
  await fs.cp(path.join(configSrc, topic), path.join(workDir, topic), { recursive: true });

  scheduleCleanup(state.currentTopicDir);
  state.currentTopicName = topic;
  state.currentTopicDir = workDir;
}

export async function getConfigs(): Promise<{ configs: string[]; current: string | null }> {
  return { configs: await listConfigs(), current: state.currentConfigName };
}

// The pipeline classifies tags only now (see `src/csv_source.rs`) and writes `<topic>.csv` — plain
// tag rows, `TAG_COLUMNS` = `osm_id,osm_type,id,category,produced,annotations,meta` (see
// `src/output/rows.rs`), no geometry. This module joins those rows back to `geometry` (the WGS84
// map `fetchWays` built from the same query that produced the pipeline's input) to reproduce the
// `Feature` shape `src/output/geojson.rs` used to build server-side — mirrors `base_properties`
// there: `osm_id`/`id` always present, `category` only when non-empty (an `accept_all` kind's
// unmatched rows have none), then `produced`/`annotations` spread in as ordinary properties.
// `topics` is always a single-element array today (exactly one topic ever runs — see
// `switchTopic`), but the merge-N-topics shape is kept as-is since `properties.topic` stamping
// still matters for `Map.tsx`'s `isolateCategory`/`focusTarget` filters. No cut points: those are a
// PBF-path/graph-edge concept (see geojson.rs) this way-only, line-geometry-only source never had.
async function buildFeatureCollections(outDir: string, topics: string[], geometry: Map<string, GeoJSON.Geometry>) {
  const features: GeoJSON.Feature[] = [];
  for (const topic of topics) {
    let text: string;
    try {
      text = await fs.readFile(path.join(outDir, `${topic}.csv`), "utf-8");
    } catch {
      continue; // topic produced no rows for this extract
    }
    const rows = parseCsv(text).slice(1); // drop the header line (osm_id,osm_type,id,category,produced,annotations,meta)
    for (const [osmId, , id, category, producedJson, annotationsJson] of rows) {
      const geom = geometry.get(osmId);
      if (!geom) continue; // shouldn't happen — every row's osm_id came from the same `geometry` map
      const properties: GeoJSON.GeoJsonProperties = { topic, osm_id: Number(osmId), id };
      if (category) properties.category = category;
      Object.assign(properties, JSON.parse(producedJson), JSON.parse(annotationsJson));
      features.push({ type: "Feature", geometry: geom, properties });
    }
  }
  return {
    type: "FeatureCollection" as const,
    features,
    cutPoints: { type: "FeatureCollection" as const, features: [] as GeoJSON.Feature[] },
  };
}

// A category file is either a single category (id = file stem) or a *family*: an object with a
// `categories` array of variants, each with a `name`, whose id is `<stem>_<name>` — mirrors
// `expand_family` in `src/topic/load.rs`. Lists every id a file produces.
async function categoryIdsInFile(filePath: string): Promise<string[]> {
  const stem = path.basename(filePath, ".json");
  let obj: unknown;
  try {
    obj = JSON.parse(await fs.readFile(filePath, "utf-8"));
  } catch {
    return [stem];
  }
  const categories = (obj as { categories?: unknown }).categories;
  if (!Array.isArray(categories)) return [stem];
  return categories
    .filter((v): v is { name: string } => typeof (v as { name?: unknown })?.name === "string")
    .map((v) => `${stem}_${v.name}`);
}

// Resolves a category id (as shown in the sidebar / stamped on classified features) back to the
// physical file that defines it — either `<id>.json` directly, or a family file whose `categories`
// array contains a variant producing that id.
async function categoryFilePath(topic: string, kind: string, name: string): Promise<string> {
  const topicDir = requireCurrentTopic(topic);
  const dir = path.join(topicDir, topic, kind);
  let entries: string[] = [];
  try {
    entries = await fs.readdir(dir);
  } catch {
    // Directory doesn't exist yet (new category being created in a fresh kind) — fall back to the
    // flat-file path so the caller's write creates it.
    return path.join(dir, `${name}.json`);
  }
  for (const entry of entries) {
    if (!entry.endsWith(".json")) continue;
    const filePath = path.join(dir, entry);
    if ((await categoryIdsInFile(filePath)).includes(name)) return filePath;
  }
  // No existing file produces this id — new category, create it flat.
  return path.join(dir, `${name}.json`);
}

// Path segments come straight from the request; a blank/malformed one would silently create a
// bogus directory (e.g. topic="" + kind="way" writes <config>/way/.json, which osmnexus then
// tries to load as a topic named "way" and fails looking for its topic.json), or escape the
// config dir entirely via "..". Block only that, not legitimate names with spaces/punctuation/
// unicode (an allowlist regex was rejecting real category names — over-eager).
export function isValidSegment(s: string): boolean {
  return s.length > 0 && s !== "." && s !== ".." && !s.includes("/") && !s.includes("\\") && !s.includes("\0");
}

// The topic-level (not per-category) JSON files a topic directory can have — `topic.json` always,
// `producers.json`/`sanitizers.json` optionally (a topic-local named producer/sanitizer library,
// merged with the config-root-level shared one on load — see `topic::load::load_topic_sanitizers`/
// `topic::runner::load`'s `producers_path` handling). Whitelisted since the file segment comes
// straight off the URL (`/api/topic/:topic/:file`).
const TOPIC_LEVEL_FILES = ["topic.json", "producers.json", "sanitizers.json"] as const;
export type TopicLevelFile = (typeof TOPIC_LEVEL_FILES)[number];

export function isTopicLevelFile(s: string): s is TopicLevelFile {
  return (TOPIC_LEVEL_FILES as readonly string[]).includes(s);
}

async function topicFilePath(topic: string, file: TopicLevelFile = "topic.json") {
  const topicDir = requireCurrentTopic(topic);
  return path.join(topicDir, topic, file);
}

// Re-runs the pipeline — against the searched-for way if one is selected (`currentWayId`,
// filtering `t.osm_id = ...` directly in `fetchWays`'s SQL), otherwise against the current
// bbox — and returns the merged multi-topic FeatureCollection. Shared by the category-edit and
// topic-config-edit endpoints (which only differ in which file they write beforehand) and by the
// way-search endpoint (`/api/way/:id/select`), which writes nothing.
export async function runPipelineAndRespond(): Promise<unknown> {
  if (!state.currentWayId && !state.currentBounds) {
    throw new ApiError(400, "no bbox selected yet: pick a bbox on the map first");
  }
  const outDir = await fs.mkdtemp(path.join(os.tmpdir(), "live-editor-"));
  const t0 = performance.now();
  const target = state.currentWayId ? { wayId: state.currentWayId } : { bounds: state.currentBounds! };
  const result = await runPipeline(target, outDir);
  const pipelineMs = Math.round(performance.now() - t0);
  if (!result.ok) {
    throw new ApiError(400, result.message);
  }
  try {
    const fc = await buildFeatureCollections(outDir, [state.currentTopicName!], result.geometry);
    return { ...fc, pipelineMs, pipelineMemKb: result.peakRssKb };
  } catch (err) {
    throw new ApiError(500, String(err));
  } finally {
    fs.rm(outDir, { recursive: true, force: true }).catch(() => {});
  }
}

export async function getCategories(topic: string): Promise<{ categories: { kind: string; name: string }[] }> {
  const topicDir = path.join(requireCurrentTopic(topic), topic);
  const kinds = ["way", "node", "relation"];
  const categories: { kind: string; name: string }[] = [];
  for (const kind of kinds) {
    let entries: string[] = [];
    try {
      entries = await fs.readdir(path.join(topicDir, kind));
    } catch {
      continue;
    }
    for (const entry of entries) {
      if (!entry.endsWith(".json")) continue;
      const ids = await categoryIdsInFile(path.join(topicDir, kind, entry));
      for (const name of ids) categories.push({ kind, name });
    }
  }
  return { categories };
}

export async function getCategory(topic: string, kind: string, name: string): Promise<{ json: string }> {
  try {
    const json = await fs.readFile(await categoryFilePath(topic, kind, name), "utf-8");
    return { json };
  } catch (err) {
    throw new ApiError(404, String(err));
  }
}

export async function deleteCategory(topic: string, kind: string, name: string): Promise<void> {
  try {
    const filePath = await categoryFilePath(topic, kind, name);
    const stem = path.basename(filePath, ".json");
    const raw = await fs.readFile(filePath, "utf-8");
    const obj = JSON.parse(raw);
    if (Array.isArray(obj.categories) && name !== stem) {
      // Family file: drop just this id's variant, keep its siblings.
      obj.categories = obj.categories.filter((v: { name?: string }) => `${stem}_${v.name}` !== name);
      if (obj.categories.length === 0) {
        await fs.unlink(filePath);
      } else {
        await fs.writeFile(filePath, JSON.stringify(obj, null, 2) + "\n", "utf-8");
      }
    } else {
      await fs.unlink(filePath);
    }
  } catch (err) {
    throw new ApiError(404, String(err));
  }
}

export async function classifyCategory(topic: string, kind: string, name: string, json: string): Promise<unknown> {
  if (![topic, kind, name].every(isValidSegment)) {
    throw new ApiError(400, "topic/kind/name must be non-empty and not '.', '..', or contain a path separator");
  }
  try {
    JSON.parse(json);
  } catch (err) {
    throw new ApiError(400, `category JSON is invalid: ${(err as Error).message}`);
  }
  const catPath = await categoryFilePath(topic, kind, name);
  await fs.mkdir(path.dirname(catPath), { recursive: true });
  await fs.writeFile(catPath, json, "utf-8");
  return runPipelineAndRespond();
}

export async function getTopicJson(topic: string, file: TopicLevelFile = "topic.json"): Promise<{ json: string }> {
  try {
    const json = await fs.readFile(await topicFilePath(topic, file), "utf-8");
    return { json };
  } catch (err) {
    // `producers.json`/`sanitizers.json` are optional (a topic with no topic-local overrides
    // just has neither file) — `topic.json` itself is required, so a missing one is a real 404.
    if (file !== "topic.json") return { json: "{}" };
    throw new ApiError(404, String(err));
  }
}

export async function setTopicJson(topic: string, json: string, file: TopicLevelFile = "topic.json"): Promise<unknown> {
  try {
    JSON.parse(json);
  } catch (err) {
    throw new ApiError(400, `${file} is invalid: ${(err as Error).message}`);
  }
  await fs.writeFile(await topicFilePath(topic, file), json, "utf-8");
  return runPipelineAndRespond();
}

// Two-stage: `list` (no `name` query param) returns just the field/kind names available for a
// topic — cheap, no graphs built. Passing `name` builds and returns just that one field's (or
// kind's) variants. `category` mode has a third level: `name` alone (a kind) returns that kind's
// category names in priority order instead of one crammed-together tree of every category at
// once; `idx` (an index into that list) then builds the single-category graph — see
// `src/bin/dag_json.rs`'s own usage doc.
export async function getDag(
  routeMode: "dag" | "categorize-dag" | "decision-tree-dag" | "sanitizer-dag",
  topic: string,
  name: string | null,
  idx: string | null,
): Promise<unknown> {
  const dagMode = { dag: "deriver", "categorize-dag": "category", "decision-tree-dag": "decision-tree", "sanitizer-dag": "sanitizer" }[routeMode];
  const topicDir = requireCurrentTopic(topic);
  const dagArgs = [topicDir, topic, dagMode, name ?? "list"];
  if (idx != null) dagArgs.push(idx);
  const result = await run(DAG_JSON_BIN, dagArgs);
  if (!result.ok) throw new ApiError(500, result.message);
  try {
    return JSON.parse(result.stdout);
  } catch (err) {
    throw new ApiError(500, `dag_json produced invalid JSON: ${String(err)}`);
  }
}
