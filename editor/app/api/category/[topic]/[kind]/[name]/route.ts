import { handle } from "@/lib/apiHandler";
import { deleteCategory, getCategory } from "@/lib/liveEditor";

type Params = { params: { topic: string; kind: string; name: string } };

function decode(params: Params["params"]) {
  return [decodeURIComponent(params.topic), decodeURIComponent(params.kind), decodeURIComponent(params.name)] as const;
}

export async function GET(_req: Request, { params }: Params) {
  return handle(() => getCategory(...decode(params)));
}

export async function DELETE(_req: Request, { params }: Params) {
  return handle(async () => {
    await deleteCategory(...decode(params));
    return { ok: true };
  });
}
