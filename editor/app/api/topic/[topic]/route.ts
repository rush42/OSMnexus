import { NextRequest } from "next/server";
import { handle } from "@/lib/apiHandler";
import { ApiError, getTopicJson, setTopicJson } from "@/lib/liveEditor";

type Params = { params: { topic: string } };

export async function GET(_req: Request, { params }: Params) {
  return handle(() => getTopicJson(decodeURIComponent(params.topic)));
}

export async function POST(req: NextRequest, { params }: Params) {
  return handle(async () => {
    let payload: { json: string };
    try {
      payload = await req.json();
    } catch {
      throw new ApiError(400, "request body is not valid JSON");
    }
    return setTopicJson(decodeURIComponent(params.topic), payload.json);
  });
}
