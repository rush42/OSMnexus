import { handle } from "@/lib/apiHandler";
import { listTopics } from "@/lib/liveEditor";

// Depends on live server state (currentConfigDir) — see app/api/bounds/route.ts's own note.
export const dynamic = "force-dynamic";

export async function GET() {
  return handle(async () => ({ topics: await listTopics() }));
}
