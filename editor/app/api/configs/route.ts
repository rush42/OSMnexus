import { handle } from "@/lib/apiHandler";
import { getConfigs } from "@/lib/liveEditor";

// Depends on live server state (currentConfigName) — see app/api/bounds/route.ts's own note.
export const dynamic = "force-dynamic";

export async function GET() {
  return handle(() => getConfigs());
}
