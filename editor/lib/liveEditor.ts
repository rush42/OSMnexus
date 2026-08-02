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
// `SOURCE_TABLE`_geom — see `configs/live_raw/topic.json` and `src/live_source.rs`). A bbox
// selection is a spatial query against this table, not an `osmium extract` + full PBF reparse.
const SOURCE_TABLE = process.env.LIVE_SOURCE_TABLE || "live_raw";
// DB connection for both the bounds query below and the pipeline subprocess (`osmnexus --source
// postgis` reads the same `PG*` env vars — see `src/config.rs`).
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
// - `currentConfigName`/`currentConfigDir`: the config directory currently being edited/run
//   against. This is always a scratch copy under the OS temp dir, never the real configs/* tree —
//   the editor copies a config into it once (on first selection, or on switching), and all
//   reads/writes/pipeline runs happen against the copy. Edits made in the live editor are
//   therefore discarded when the dev server restarts and never touch the repo's actual configs/*.
const globalState = globalThis as unknown as {
  __liveEditor?: {
    currentBounds: [number, number, number, number] | null;
    currentConfigName: string | null;
    currentConfigDir: string | null;
  };
};
const state = (globalState.__liveEditor ??= { currentBounds: null, currentConfigName: null, currentConfigDir: null });

// A config is any non-`_`-prefixed directory directly under CONFIGS_ROOT (configs/tilda,
// configs/osmnx, configs/public_transport, ...) — same discovery rule as listTopics() below, one
// level up.
export async function listConfigs(): Promise<string[]> {
  const entries = await fs.readdir(CONFIGS_ROOT, { withFileTypes: true });
  return entries
    .filter((e) => e.isDirectory() && !e.name.startsWith("_"))
    .map((e) => e.name)
    .sort();
}

async function loadConfigCopy(name: string): Promise<string> {
  const workDir = await fs.mkdtemp(path.join(os.tmpdir(), "live-editor-config-"));
  const dest = path.join(workDir, name);
  await fs.cp(path.join(CONFIGS_ROOT, name), dest, { recursive: true });
  return dest;
}

export async function ensureConfigSelected(): Promise<string> {
  if (state.currentConfigDir) return state.currentConfigDir;
  const configs = await listConfigs();
  state.currentConfigName = configs[0];
  state.currentConfigDir = await loadConfigCopy(configs[0]);
  return state.currentConfigDir;
}

function run(bin: string, args: string[], env?: Record<string, string>): Promise<{ ok: true; stdout: string } | { ok: false; message: string }> {
  return new Promise((resolve) => {
    const child = spawn(bin, args, { stdio: ["ignore", "pipe", "pipe"], env: env ? { ...process.env, ...env } : process.env });
    let stdout = "";
    let stderr = "";
    child.stdout.on("data", (d) => (stdout += d.toString()));
    child.stderr.on("data", (d) => (stderr += d.toString()));
    child.on("error", (err) => resolve({ ok: false, message: String(err) }));
    child.on("close", (code) => {
      if (code === 0) resolve({ ok: true, stdout });
      else resolve({ ok: false, message: stderr || `${bin} exited with code ${code}` });
    });
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
export function selectBbox(bounds: [number, number, number, number]): { bounds: [number, number, number, number] } {
  state.currentBounds = bounds;
  return { bounds: state.currentBounds };
}

async function runPipeline(bounds: [number, number, number, number], outDir: string): Promise<{ ok: true } | { ok: false; message: string }> {
  const configDir = await ensureConfigSelected();
  const result = await run(
    PIPELINE_BIN,
    [
      "--source",
      "postgis",
      "--source-table",
      SOURCE_TABLE,
      "--bbox",
      bounds.join(","),
      "--config-dir",
      configDir,
      "--output",
      "geojsonseq",
      "--out-dir",
      outDir,
      "--linear-classify",
    ],
    PG_ENV,
  );
  return result.ok ? { ok: true } : result;
}

// A topic is any non-`_`-prefixed directory directly under the current config dir — same
// discovery rule the Rust side uses (see TopicRunner::load_all), so nothing here needs to
// hardcode topic names.
export async function listTopics(): Promise<string[]> {
  const configDir = await ensureConfigSelected();
  const entries = await fs.readdir(configDir, { withFileTypes: true });
  return entries
    .filter((e) => e.isDirectory() && !e.name.startsWith("_"))
    .map((e) => e.name)
    .sort();
}

export async function switchConfig(config: string): Promise<void> {
  if (!(await listConfigs()).includes(config)) {
    throw new ApiError(400, `unknown config '${config}'`);
  }
  state.currentConfigName = config;
  state.currentConfigDir = await loadConfigCopy(config);
}

export async function getConfigs(): Promise<{ configs: string[]; current: string | null }> {
  await ensureConfigSelected();
  return { configs: await listConfigs(), current: state.currentConfigName };
}

// The pipeline itself joins tag rows to edge geometries (by osm_id) and reprojects back to WGS84
// — see src/output/geojson.rs — so this just reads its per-topic output back (a newline-delimited
// GeoJSON Feature stream, RFC 8142) and merges every topic into one FeatureCollection, stamping a
// `topic` property onto each feature/cut point since category *names* aren't unique across topics
// (e.g. osmnx's bike/walk/drive all use "all"). Cut points are interleaved into the same stream,
// tagged `properties.kind` of `"cut"`/`"endpoint"` (see geojson.rs), and split back out here.
async function readMergedFeatureCollections(outDir: string, topics: string[]) {
  const features: GeoJSON.Feature[] = [];
  const cutPoints: GeoJSON.Feature[] = [];
  for (const topic of topics) {
    let text: string;
    try {
      text = await fs.readFile(path.join(outDir, `${topic}.geojsonseq`), "utf-8");
    } catch {
      continue; // topic produced no rows for this extract
    }
    for (const line of text.split("\n")) {
      if (!line.trim()) continue;
      const f: GeoJSON.Feature = JSON.parse(line);
      const stamped = { ...f, properties: { topic, ...f.properties } };
      if (f.properties?.kind === "cut" || f.properties?.kind === "endpoint") {
        cutPoints.push(stamped);
      } else {
        features.push(stamped);
      }
    }
  }
  return {
    type: "FeatureCollection" as const,
    features,
    cutPoints: { type: "FeatureCollection" as const, features: cutPoints },
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
  const configDir = await ensureConfigSelected();
  const dir = path.join(configDir, topic, kind);
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

async function topicFilePath(topic: string) {
  const configDir = await ensureConfigSelected();
  return path.join(configDir, topic, "topic.json");
}

// Re-runs the pipeline against the current bbox and returns the merged multi-topic
// FeatureCollection — shared by the category-edit and topic-config-edit endpoints, which only
// differ in which file they write beforehand.
export async function runPipelineAndRespond(): Promise<unknown> {
  if (!state.currentBounds) {
    throw new ApiError(400, "no bbox selected yet: pick a bbox on the map first");
  }
  const outDir = await fs.mkdtemp(path.join(os.tmpdir(), "live-editor-"));
  const t0 = performance.now();
  const result = await runPipeline(state.currentBounds, outDir);
  const pipelineMs = Math.round(performance.now() - t0);
  if (!result.ok) {
    throw new ApiError(400, result.message);
  }
  try {
    const fc = await readMergedFeatureCollections(outDir, await listTopics());
    return { ...fc, pipelineMs };
  } catch (err) {
    throw new ApiError(500, String(err));
  } finally {
    fs.rm(outDir, { recursive: true, force: true }).catch(() => {});
  }
}

export async function getCategories(topic: string): Promise<{ categories: { kind: string; name: string }[] }> {
  const topicDir = path.join(await ensureConfigSelected(), topic);
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
  if (!(await listTopics()).includes(topic)) {
    throw new ApiError(400, `unknown topic '${topic}'`);
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

export async function getTopicJson(topic: string): Promise<{ json: string }> {
  try {
    const json = await fs.readFile(await topicFilePath(topic), "utf-8");
    return { json };
  } catch (err) {
    throw new ApiError(404, String(err));
  }
}

export async function setTopicJson(topic: string, json: string): Promise<unknown> {
  if (!(await listTopics()).includes(topic)) {
    throw new ApiError(400, `unknown topic '${topic}'`);
  }
  try {
    JSON.parse(json);
  } catch (err) {
    throw new ApiError(400, `topic JSON is invalid: ${(err as Error).message}`);
  }
  await fs.writeFile(await topicFilePath(topic), json, "utf-8");
  return runPipelineAndRespond();
}

// Two-stage: `list` (no `name` query param) returns just the field/kind names available for a
// topic — cheap, no graphs built. Passing `name` builds and returns just that one field's (or
// kind's) variants. `category` mode has a third level: `name` alone (a kind) returns that kind's
// category names in priority order instead of one crammed-together tree of every category at
// once; `idx` (an index into that list) then builds the single-category graph — see
// `src/bin/dag_json.rs`'s own usage doc.
export async function getDag(routeMode: "dag" | "categorize-dag" | "decision-tree-dag", topic: string, name: string | null, idx: string | null): Promise<unknown> {
  const dagMode = { dag: "deriver", "categorize-dag": "category", "decision-tree-dag": "decision-tree" }[routeMode];
  if (!(await listTopics()).includes(topic)) {
    throw new ApiError(400, `unknown topic '${topic}'`);
  }
  const configDir = await ensureConfigSelected();
  const dagArgs = [configDir, topic, dagMode, name ?? "list"];
  if (idx != null) dagArgs.push(idx);
  const result = await run(DAG_JSON_BIN, dagArgs);
  if (!result.ok) throw new ApiError(500, result.message);
  try {
    return JSON.parse(result.stdout);
  } catch (err) {
    throw new ApiError(500, `dag_json produced invalid JSON: ${String(err)}`);
  }
}
