import { handle } from "@/lib/apiHandler";
import { searchAndRunWay } from "@/lib/liveEditor";

type Params = { params: { id: string } };

// Depends on a live DB query + pipeline subprocess — must run per-request.
export const dynamic = "force-dynamic";

export async function GET(_req: Request, { params }: Params) {
  return handle(() => searchAndRunWay(decodeURIComponent(params.id)));
}
