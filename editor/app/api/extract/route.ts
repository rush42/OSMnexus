import { NextRequest } from "next/server";
import { handle } from "@/lib/apiHandler";
import { ApiError, selectBbox } from "@/lib/liveEditor";

// Historically this shelled out to `osmium extract` (cut a PBF slice); now it's just recording the
// bbox — the pipeline queries the postgis source table for exactly this bbox on every run instead
// (see `lib/liveEditor.ts`'s `runPipeline`). Endpoint path/shape kept the same so the frontend
// doesn't need to change.
export async function POST(req: NextRequest) {
  return handle(async () => {
    let payload: { bounds: [number, number, number, number] };
    try {
      payload = await req.json();
    } catch {
      throw new ApiError(400, "request body is not valid JSON");
    }
    const bounds = payload.bounds;
    if (!Array.isArray(bounds) || bounds.length !== 4 || bounds.some((n) => typeof n !== "number")) {
      throw new ApiError(400, "bounds must be [west, south, east, north]");
    }
    const t0 = performance.now();
    const result = selectBbox(bounds as [number, number, number, number]);
    const extractMs = Math.round(performance.now() - t0);
    return { ...result, extractMs };
  });
}
