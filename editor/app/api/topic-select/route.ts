import { NextRequest } from "next/server";
import { handle } from "@/lib/apiHandler";
import { ApiError, switchTopic } from "@/lib/liveEditor";

// Named `topic-select` (not `topic`) to avoid colliding with the existing dynamic
// `app/api/topic/[topic]/route.ts` (topic.json GET/POST) — matches the hyphenated-segment
// convention already used by `categorize-dag`/`decision-tree-dag`/`sanitizer-dag`.
export async function POST(req: NextRequest) {
  return handle(async () => {
    let payload: { topic: string };
    try {
      payload = await req.json();
    } catch {
      throw new ApiError(400, "request body is not valid JSON");
    }
    await switchTopic(payload.topic);
    return { ok: true };
  });
}
