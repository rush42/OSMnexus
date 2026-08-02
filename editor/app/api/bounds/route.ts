import { handle } from "@/lib/apiHandler";
import { getBounds } from "@/lib/liveEditor";

// Depends on live server state (currentBounds) and a live DB query — must run per-request, not be
// statically rendered/cached at build time (Next.js's default for a param-less GET route).
export const dynamic = "force-dynamic";

export async function GET() {
  return handle(() => getBounds());
}
