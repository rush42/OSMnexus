import { NextRequest } from "next/server";
import { handle } from "@/lib/apiHandler";
import { ApiError, classifyCategory } from "@/lib/liveEditor";

export async function POST(req: NextRequest) {
  return handle(async () => {
    let payload: { topic: string; kind: string; name: string; json: string };
    try {
      payload = await req.json();
    } catch {
      throw new ApiError(400, "request body is not valid JSON");
    }
    return classifyCategory(payload.topic, payload.kind, payload.name, payload.json);
  });
}
