import { handle } from "@/lib/apiHandler";
import { getCategories } from "@/lib/liveEditor";

export async function GET(_req: Request, { params }: { params: { topic: string } }) {
  return handle(() => getCategories(decodeURIComponent(params.topic)));
}
