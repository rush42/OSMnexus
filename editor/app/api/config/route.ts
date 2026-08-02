import { NextRequest } from "next/server";
import { handle } from "@/lib/apiHandler";
import { ApiError, switchConfig } from "@/lib/liveEditor";

export async function POST(req: NextRequest) {
  return handle(async () => {
    let payload: { config: string };
    try {
      payload = await req.json();
    } catch {
      throw new ApiError(400, "request body is not valid JSON");
    }
    await switchConfig(payload.config);
    return { ok: true };
  });
}
