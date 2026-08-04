import { NextRequest } from "next/server";
import { handle } from "@/lib/apiHandler";
import { ApiError, getTopicJson, isTopicLevelFile, setTopicJson } from "@/lib/liveEditor";

type Params = { params: { topic: string; file: string } };

function requireFile(raw: string) {
  const file = decodeURIComponent(raw);
  if (!isTopicLevelFile(file)) {
    throw new ApiError(404, `unknown topic-level file '${file}' (expected topic.json, producers.json, or sanitizers.json)`);
  }
  return file;
}

export async function GET(_req: Request, { params }: Params) {
  return handle(() => getTopicJson(decodeURIComponent(params.topic), requireFile(params.file)));
}

export async function POST(req: NextRequest, { params }: Params) {
  return handle(async () => {
    let payload: { json: string };
    try {
      payload = await req.json();
    } catch {
      throw new ApiError(400, "request body is not valid JSON");
    }
    return setTopicJson(decodeURIComponent(params.topic), payload.json, requireFile(params.file));
  });
}
