import { defineConfig, type Plugin } from "vite";
import react from "@vitejs/plugin-react";
import { spawn } from "node:child_process";
import { promises as fs } from "node:fs";
import path from "node:path";
import os from "node:os";

const EDITOR_DIR = path.resolve(__dirname);
const REPO_DIR = path.resolve(__dirname, "..");
// Defaults to the image's baked-in build (see editor/Dockerfile); override to point at a
// host-built target/release/osmnexus instead, for iterating on Rust code without a full image
// rebuild (e.g. `PIPELINE_BIN_PATH=/repo/target/release/osmnexus docker compose up`).
const PIPELINE_BIN = process.env.PIPELINE_BIN_PATH || path.join(REPO_DIR, "target", "release", "osmnexus");
// Emits a topic's output Producer trees as node/edge JSON for the tree view — see `src/bin/dag_json.rs`.
const DAG_JSON_BIN = process.env.DAG_JSON_BIN_PATH || path.join(REPO_DIR, "target", "release", "dag_json");
const CONFIGS_ROOT = path.join(REPO_DIR, "configs");
// The table an "all ways" pass loaded a whole region into (tags in `SOURCE_TABLE`, geometry in
// `SOURCE_TABLE`_geom — see `configs/live_raw/topic.json` and `src/live_source.rs`). A bbox
// selection is now just a spatial query against this table, not an `osmium extract` + full PBF
// reparse — see this file's own former `extractBbox`/`runPipeline`.
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
const MAX_BBOX_M = Number(process.env.MAX_BBOX_M) || 10000;

// The bbox currently in use for the map/pipeline. Starts out unset until the user picks one.
let currentBounds: [number, number, number, number] | null = null;

// A config is any non-`_`-prefixed directory directly under CONFIGS_ROOT (configs/tilda,
// configs/osmnx, configs/public_transport, ...) — same discovery rule as listTopics() below, one
// level up.
async function listConfigs(): Promise<string[]> {
  const entries = await fs.readdir(CONFIGS_ROOT, { withFileTypes: true });
  return entries
    .filter((e) => e.isDirectory() && !e.name.startsWith("_"))
    .map((e) => e.name)
    .sort();
}

// The config directory currently being edited/run against. This is always a scratch copy under
// the OS temp dir, never the real configs/* tree — the editor copies a config into it once (on
// first selection, or on switching), and all reads/writes/pipeline runs happen against the copy.
// Edits made in the live editor are therefore discarded when the dev server restarts and never
// touch the repo's actual configs/*.
let currentConfigName: string | null = null;
let currentConfigDir: string | null = null;

async function loadConfigCopy(name: string): Promise<string> {
  const workDir = await fs.mkdtemp(path.join(os.tmpdir(), "live-editor-config-"));
  const dest = path.join(workDir, name);
  await fs.cp(path.join(CONFIGS_ROOT, name), dest, { recursive: true });
  return dest;
}

async function ensureConfigSelected(): Promise<string> {
  if (currentConfigDir) return currentConfigDir;
  const configs = await listConfigs();
  currentConfigName = configs[0];
  currentConfigDir = await loadConfigCopy(configs[0]);
  return currentConfigDir;
}

function run(bin: string, args: string[], env?: NodeJS.ProcessEnv): Promise<{ ok: true; stdout: string } | { ok: false; message: string }> {
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

// Selecting a bbox used to shell out to `osmium extract` (cut a fresh PBF slice, then re-parse it
// from scratch on every single edit). Now it's just recording the bounds — the pipeline queries
// `SOURCE_TABLE`/`SOURCE_TABLE`_geom for exactly this bbox on every run instead (see `runPipeline`).
function selectBbox(bounds: [number, number, number, number]): void {
  currentBounds = bounds;
}

function readBody(req: any): Promise<string> {
  return new Promise((resolve, reject) => {
    let data = "";
    req.on("data", (chunk: Buffer) => (data += chunk));
    req.on("end", () => resolve(data));
    req.on("error", reject);
  });
}

function sendJson(res: any, status: number, body: unknown) {
  const text = JSON.stringify(body);
  res.statusCode = status;
  res.setHeader("Content-Type", "application/json");
  res.end(text);
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
      "geojson",
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
async function listTopics(): Promise<string[]> {
  const configDir = await ensureConfigSelected();
  const entries = await fs.readdir(configDir, { withFileTypes: true });
  return entries
    .filter((e) => e.isDirectory() && !e.name.startsWith("_"))
    .map((e) => e.name)
    .sort();
}

// The pipeline itself joins tag rows to edge geometries (by osm_id) and reprojects back to WGS84
// — see src/output/geojson.rs — so this just reads its per-topic output back and merges every
// topic into one FeatureCollection, stamping a `topic` property onto each feature/cut point since
// category *names* aren't unique across topics (e.g. osmnx's bike/walk/drive all use "all").
async function readMergedFeatureCollections(outDir: string, topics: string[]) {
  const features: GeoJSON.Feature[] = [];
  const cutPoints: GeoJSON.Feature[] = [];
  for (const topic of topics) {
    let fc: { features: GeoJSON.Feature[]; cutPoints?: { features: GeoJSON.Feature[] } };
    try {
      fc = JSON.parse(await fs.readFile(path.join(outDir, `${topic}.geojson`), "utf-8"));
    } catch {
      continue; // topic produced no rows for this extract
    }
    for (const f of fc.features) {
      features.push({ ...f, properties: { topic, ...f.properties } });
    }
    for (const f of fc.cutPoints?.features ?? []) {
      cutPoints.push({ ...f, properties: { topic, ...f.properties } });
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
function isValidSegment(s: string): boolean {
  return s.length > 0 && s !== "." && s !== ".." && !s.includes("/") && !s.includes("\\") && !s.includes("\0");
}

async function topicFilePath(topic: string) {
  const configDir = await ensureConfigSelected();
  return path.join(configDir, topic, "topic.json");
}

// Re-runs the pipeline against the current extract and replies with the merged multi-topic
// FeatureCollection — shared by the category-edit and topic-config-edit endpoints, which only
// differ in which file they write beforehand.
async function runPipelineAndRespond(res: any) {
  if (!currentBounds) {
    return sendJson(res, 400, { error: "no bbox selected yet: pick a bbox on the map first" });
  }
  const outDir = await fs.mkdtemp(path.join(os.tmpdir(), "live-editor-"));
  const t0 = performance.now();
  const result = await runPipeline(currentBounds, outDir);
  const pipelineMs = Math.round(performance.now() - t0);
  if (!result.ok) {
    return sendJson(res, 400, { error: result.message });
  }
  try {
    const fc = await readMergedFeatureCollections(outDir, await listTopics());
    return sendJson(res, 200, { ...fc, pipelineMs });
  } catch (err) {
    return sendJson(res, 500, { error: String(err) });
  } finally {
    fs.rm(outDir, { recursive: true, force: true }).catch(() => {});
  }
}

function liveEditorApi(): Plugin {
  return {
    name: "live-editor-api",
    configureServer(server) {
      server.middlewares.use(async (req, res, next) => {
        if (!req.url) return next();
        const url = new URL(req.url, "http://localhost");

        if (url.pathname === "/api/bounds" && req.method === "GET") {
          if (currentBounds) return sendJson(res, 200, { bounds: currentBounds, selected: true, maxBboxM: MAX_BBOX_M });
          try {
            const bounds = await baseTableBounds();
            return sendJson(res, 200, { bounds, selected: false, maxBboxM: MAX_BBOX_M });
          } catch (err) {
            return sendJson(res, 500, { error: String(err) });
          }
        }

        // Historically this shelled out to `osmium extract` (cut a PBF slice); now it's just
        // recording the bbox — see `selectBbox`'s own doc. Endpoint path/shape kept the same so the
        // frontend doesn't need to change.
        if (url.pathname === "/api/extract" && req.method === "POST") {
          const body = await readBody(req);
          let payload: { bounds: [number, number, number, number] };
          try {
            payload = JSON.parse(body);
          } catch {
            return sendJson(res, 400, { error: "request body is not valid JSON" });
          }
          const bounds = payload.bounds;
          if (!Array.isArray(bounds) || bounds.length !== 4 || bounds.some((n) => typeof n !== "number")) {
            return sendJson(res, 400, { error: "bounds must be [west, south, east, north]" });
          }
          const t0 = performance.now();
          selectBbox(bounds as [number, number, number, number]);
          const extractMs = Math.round(performance.now() - t0);
          return sendJson(res, 200, { bounds: currentBounds, extractMs });
        }

        if (url.pathname === "/api/configs" && req.method === "GET") {
          await ensureConfigSelected();
          return sendJson(res, 200, { configs: await listConfigs(), current: currentConfigName });
        }

        if (url.pathname === "/api/config" && req.method === "POST") {
          const body = await readBody(req);
          let payload: { config: string };
          try {
            payload = JSON.parse(body);
          } catch {
            return sendJson(res, 400, { error: "request body is not valid JSON" });
          }
          if (!(await listConfigs()).includes(payload.config)) {
            return sendJson(res, 400, { error: `unknown config '${payload.config}'` });
          }
          currentConfigName = payload.config;
          currentConfigDir = await loadConfigCopy(payload.config);
          return sendJson(res, 200, { ok: true });
        }

        if (url.pathname === "/api/topics" && req.method === "GET") {
          return sendJson(res, 200, { topics: await listTopics() });
        }

        // Two-stage: `list` (no `name` query param) returns just the field/kind names available for
        // a topic — cheap, no graphs built. Passing `name` builds and returns just that one field's
        // (or kind's) variants. dag_json used to build every field/kind's graph on every request,
        // even though the live editor only ever displays one at a time — the dominant cost for
        // topics with many/large fields, so the frontend now fetches the list once and a graph per
        // field the user actually selects. `category` mode has a third level: `name` alone (a kind)
        // returns that kind's category names in priority order instead of one crammed-together tree
        // of every category at once; `idx` (an index into that list) then builds the single-category
        // graph — see `src/bin/dag_json.rs`'s own usage doc.
        const dagRouteMatch = url.pathname.match(/^\/api\/(dag|categorize-dag|decision-tree-dag)\/([^/]+)$/);
        if (dagRouteMatch && req.method === "GET") {
          const dagMode = { dag: "deriver", "categorize-dag": "category", "decision-tree-dag": "decision-tree" }[dagRouteMatch[1]]!;
          const topic = decodeURIComponent(dagRouteMatch[2]);
          if (!(await listTopics()).includes(topic)) {
            return sendJson(res, 400, { error: `unknown topic '${topic}'` });
          }
          const name = url.searchParams.get("name");
          const idx = url.searchParams.get("idx");
          const configDir = await ensureConfigSelected();
          const dagArgs = [configDir, topic, dagMode, name ?? "list"];
          if (idx != null) dagArgs.push(idx);
          const result = await run(DAG_JSON_BIN, dagArgs);
          if (!result.ok) return sendJson(res, 500, { error: result.message });
          try {
            return sendJson(res, 200, JSON.parse(result.stdout));
          } catch (err) {
            return sendJson(res, 500, { error: `dag_json produced invalid JSON: ${String(err)}` });
          }
        }

        const categoriesMatch = url.pathname.match(/^\/api\/categories\/([^/]+)$/);
        if (categoriesMatch && req.method === "GET") {
          const topic = decodeURIComponent(categoriesMatch[1]);
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
          return sendJson(res, 200, { categories });
        }

        const categoryMatch = url.pathname.match(/^\/api\/category\/([^/]+)\/([^/]+)\/([^/]+)$/);
        if (categoryMatch && req.method === "GET") {
          const [topic, kind, name] = categoryMatch.slice(1).map(decodeURIComponent);
          try {
            const json = await fs.readFile(await categoryFilePath(topic, kind, name), "utf-8");
            return sendJson(res, 200, { json });
          } catch (err) {
            return sendJson(res, 404, { error: String(err) });
          }
        }

        if (categoryMatch && req.method === "DELETE") {
          const [topic, kind, name] = categoryMatch.slice(1).map(decodeURIComponent);
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
            return sendJson(res, 200, { ok: true });
          } catch (err) {
            return sendJson(res, 404, { error: String(err) });
          }
        }

        if (url.pathname === "/api/classify" && req.method === "POST") {
          const body = await readBody(req);
          let payload: { topic: string; kind: string; name: string; json: string };
          try {
            payload = JSON.parse(body);
          } catch {
            return sendJson(res, 400, { error: "request body is not valid JSON" });
          }
          const { topic, kind, name, json } = payload;

          if (![topic, kind, name].every(isValidSegment)) {
            return sendJson(res, 400, { error: "topic/kind/name must be non-empty and not '.', '..', or contain a path separator" });
          }
          if (!(await listTopics()).includes(topic)) {
            return sendJson(res, 400, { error: `unknown topic '${topic}'` });
          }

          try {
            JSON.parse(json);
          } catch (err) {
            return sendJson(res, 400, { error: `category JSON is invalid: ${(err as Error).message}` });
          }

          const catPath = await categoryFilePath(topic, kind, name);
          await fs.mkdir(path.dirname(catPath), { recursive: true });
          await fs.writeFile(catPath, json, "utf-8");

          return runPipelineAndRespond(res);
        }

        const topicMatch = url.pathname.match(/^\/api\/topic\/([^/]+)$/);
        if (topicMatch && req.method === "GET") {
          const topic = decodeURIComponent(topicMatch[1]);
          try {
            const json = await fs.readFile(await topicFilePath(topic), "utf-8");
            return sendJson(res, 200, { json });
          } catch (err) {
            return sendJson(res, 404, { error: String(err) });
          }
        }

        if (topicMatch && req.method === "POST") {
          const topic = decodeURIComponent(topicMatch[1]);
          if (!(await listTopics()).includes(topic)) {
            return sendJson(res, 400, { error: `unknown topic '${topic}'` });
          }
          const body = await readBody(req);
          let payload: { json: string };
          try {
            payload = JSON.parse(body);
          } catch {
            return sendJson(res, 400, { error: "request body is not valid JSON" });
          }

          try {
            JSON.parse(payload.json);
          } catch (err) {
            return sendJson(res, 400, { error: `topic JSON is invalid: ${(err as Error).message}` });
          }

          await fs.writeFile(await topicFilePath(topic), payload.json, "utf-8");
          return runPipelineAndRespond(res);
        }

        next();
      });
    },
  };
}

export default defineConfig({
  plugins: [react(), liveEditorApi()],
  server: {
    port: 5173,
    strictPort: true,
  },
});
