import { defineConfig, type Plugin } from "vite";
import react from "@vitejs/plugin-react";
import { spawn } from "node:child_process";
import { promises as fs } from "node:fs";
import path from "node:path";
import os from "node:os";
import wkx from "wkx";

const EDITOR_DIR = path.resolve(__dirname);
const REPO_DIR = path.resolve(__dirname, "..");
const PIPELINE_BIN = path.join(REPO_DIR, "target", "release", "osmnexus");
const LIVE_CONFIG_DIR = path.join(EDITOR_DIR, "live-config");
const BASE_PBF = process.env.BASE_PBF_PATH || path.join(EDITOR_DIR, "fixtures", "tiny.osm.pbf");
const EXTRACT_DIR = path.join(EDITOR_DIR, "live-extract");

// The extract currently in use for the map/pipeline. Starts out unset until
// the user picks a bbox (or, for the bundled fixture, defaults to it).
let currentExtractPath: string | null = null;
let currentBounds: [number, number, number, number] | null = null;

function run(bin: string, args: string[]): Promise<{ ok: true; stdout: string } | { ok: false; message: string }> {
  return new Promise((resolve) => {
    const child = spawn(bin, args, { stdio: ["ignore", "pipe", "pipe"] });
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

async function baseFileBounds(): Promise<[number, number, number, number]> {
  const result = await run("osmium", ["fileinfo", "-e", "-j", BASE_PBF]);
  if (!result.ok) throw new Error(result.message);
  const info = JSON.parse(result.stdout);
  return info.data.bbox;
}

async function extractBbox(bounds: [number, number, number, number]): Promise<{ ok: true } | { ok: false; message: string }> {
  await fs.mkdir(EXTRACT_DIR, { recursive: true });
  const outPath = path.join(EXTRACT_DIR, "extract.osm.pbf");
  const result = await run("osmium", [
    "extract",
    "-b",
    bounds.join(","),
    "-s",
    "complete_ways",
    "-o",
    outPath,
    "--overwrite",
    BASE_PBF,
  ]);
  if (!result.ok) return result;
  currentExtractPath = outPath;
  currentBounds = bounds;
  return { ok: true };
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

async function runPipeline(pbfPath: string, outDir: string): Promise<{ ok: true } | { ok: false; message: string }> {
  const result = await run(PIPELINE_BIN, [pbfPath, "--config-dir", LIVE_CONFIG_DIR, "--output", "csv", "--out-dir", outDir]);
  return result.ok ? { ok: true } : result;
}

// Minimal CSV parser: no embedded newlines in fields except JSON-stringified
// columns, which never contain raw newlines (serde_json escapes them).
function parseCsv(text: string): Record<string, string>[] {
  const lines = text.split("\n").filter((l) => l.length > 0);
  if (lines.length === 0) return [];
  const header = splitCsvLine(lines[0]);
  return lines.slice(1).map((line) => {
    const cols = splitCsvLine(line);
    const row: Record<string, string> = {};
    header.forEach((h, i) => (row[h] = cols[i] ?? ""));
    return row;
  });
}

function splitCsvLine(line: string): string[] {
  const out: string[] = [];
  let cur = "";
  let inQuotes = false;
  for (let i = 0; i < line.length; i++) {
    const c = line[i];
    if (inQuotes) {
      if (c === '"') {
        if (line[i + 1] === '"') {
          cur += '"';
          i++;
        } else {
          inQuotes = false;
        }
      } else {
        cur += c;
      }
    } else {
      if (c === '"') inQuotes = true;
      else if (c === ",") {
        out.push(cur);
        cur = "";
      } else cur += c;
    }
  }
  out.push(cur);
  return out;
}

const R_MAJOR = 20037508.34;

// geometries.csv stores geometry reprojected to EPSG:3857 (see src/output/geometry.rs
// wgs84_to_3857, used for the Postgres/tile-server output path) — convert back to
// WGS84 lon/lat for GeoJSON.
function mercatorToLonLat(coord: number[]): number[] {
  const [x, y] = coord;
  const lon = (x / R_MAJOR) * 180;
  const lat = (180 / Math.PI) * (2 * Math.atan(Math.exp((y / R_MAJOR) * Math.PI)) - Math.PI / 2);
  return [lon, lat];
}

function reprojectGeometry(geometry: any): any {
  if (typeof geometry.coordinates === "undefined") return geometry;
  const reprojectDeep = (coords: any): any =>
    typeof coords[0] === "number" ? mercatorToLonLat(coords) : coords.map(reprojectDeep);
  return { ...geometry, coordinates: reprojectDeep(geometry.coordinates) };
}

async function buildFeatureCollection(outDir: string, topic: string) {
  const [topicCsv, geomCsv] = await Promise.all([
    fs.readFile(path.join(outDir, `${topic}.csv`), "utf-8"),
    fs.readFile(path.join(outDir, "edges.csv"), "utf-8"),
  ]);
  const topicRows = parseCsv(topicCsv);
  const geomRows = parseCsv(geomCsv);
  const geomByOsmId = new Map<string, string>();
  for (const row of geomRows) {
    if (row.geom) geomByOsmId.set(row.osm_id, row.geom);
  }

  const features = [];
  for (const row of topicRows) {
    const hex = geomByOsmId.get(row.osm_id);
    if (!hex) continue;
    let geometry;
    try {
      const geom = wkx.Geometry.parse(Buffer.from(hex, "hex"));
      geometry = reprojectGeometry(geom.toGeoJSON());
    } catch {
      continue;
    }
    let osm = {};
    let derived = {};
    try {
      osm = JSON.parse(row.osm || "{}");
    } catch {
      /* ignore */
    }
    try {
      derived = JSON.parse(row.derived || "{}");
    } catch {
      /* ignore */
    }
    features.push({
      type: "Feature",
      geometry,
      properties: { osm_id: row.osm_id, id: row.id, ...osm, ...derived },
    });
  }
  return { type: "FeatureCollection", features };
}

function categoryFilePath(topic: string, kind: string, name: string) {
  return path.join(LIVE_CONFIG_DIR, topic, kind, `${name}.json`);
}

function liveEditorApi(): Plugin {
  return {
    name: "live-editor-api",
    configureServer(server) {
      server.middlewares.use(async (req, res, next) => {
        if (!req.url) return next();
        const url = new URL(req.url, "http://localhost");

        if (url.pathname === "/api/bounds" && req.method === "GET") {
          if (currentBounds) return sendJson(res, 200, { bounds: currentBounds, selected: true });
          try {
            const bounds = await baseFileBounds();
            return sendJson(res, 200, { bounds, selected: false });
          } catch (err) {
            return sendJson(res, 500, { error: String(err) });
          }
        }

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
          const result = await extractBbox(bounds as [number, number, number, number]);
          if (!result.ok) return sendJson(res, 400, { error: result.message });
          return sendJson(res, 200, { bounds: currentBounds });
        }

        const categoriesMatch = url.pathname.match(/^\/api\/categories\/([^/]+)$/);
        if (categoriesMatch && req.method === "GET") {
          const [, topic] = categoriesMatch;
          const topicDir = path.join(LIVE_CONFIG_DIR, topic);
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
              if (entry.endsWith(".json")) categories.push({ kind, name: entry.slice(0, -".json".length) });
            }
          }
          return sendJson(res, 200, { categories });
        }

        const categoryMatch = url.pathname.match(/^\/api\/category\/([^/]+)\/([^/]+)\/([^/]+)$/);
        if (categoryMatch && req.method === "GET") {
          const [, topic, kind, name] = categoryMatch;
          try {
            const json = await fs.readFile(categoryFilePath(topic, kind, name), "utf-8");
            return sendJson(res, 200, { json });
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

          if (!currentExtractPath) {
            return sendJson(res, 400, { error: "no extract selected yet: pick a bbox on the map first" });
          }

          try {
            JSON.parse(json);
          } catch (err) {
            return sendJson(res, 400, { error: `category JSON is invalid: ${(err as Error).message}` });
          }

          await fs.mkdir(path.dirname(categoryFilePath(topic, kind, name)), { recursive: true });
          await fs.writeFile(categoryFilePath(topic, kind, name), json, "utf-8");

          const outDir = await fs.mkdtemp(path.join(os.tmpdir(), "live-editor-"));
          const result = await runPipeline(currentExtractPath, outDir);
          if (!result.ok) {
            return sendJson(res, 400, { error: result.message });
          }
          try {
            const fc = await buildFeatureCollection(outDir, topic);
            return sendJson(res, 200, fc);
          } catch (err) {
            return sendJson(res, 500, { error: String(err) });
          } finally {
            fs.rm(outDir, { recursive: true, force: true }).catch(() => {});
          }
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
