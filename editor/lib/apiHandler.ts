import { NextResponse } from "next/server";
import { ApiError } from "./liveEditor";

// Every route handler's body is `() => Promise<unknown>` — this runs it, JSON-encodes a plain
// return value as a 200, and turns an `ApiError` into its declared status (or 500 for anything
// else unexpected, e.g. a raw fs/child_process rejection nobody wrapped).
export async function handle(fn: () => Promise<unknown>): Promise<NextResponse> {
  try {
    const body = await fn();
    return NextResponse.json(body ?? { ok: true });
  } catch (err) {
    if (err instanceof ApiError) {
      return NextResponse.json({ error: err.message }, { status: err.status });
    }
    return NextResponse.json({ error: String(err) }, { status: 500 });
  }
}
