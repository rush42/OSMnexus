import { NextRequest } from "next/server";
import { handle } from "@/lib/apiHandler";
import { getDag } from "@/lib/liveEditor";

export async function GET(req: NextRequest, { params }: { params: { topic: string } }) {
  return handle(() =>
    getDag("sanitizer-dag", decodeURIComponent(params.topic), req.nextUrl.searchParams.get("name"), req.nextUrl.searchParams.get("idx")),
  );
}
