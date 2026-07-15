import { useEffect, useMemo, useState } from "react";
import { ReactFlow, Background, Controls, type Node, type Edge } from "@xyflow/react";
import "@xyflow/react/dist/style.css";

type DagNode = { id: string; label: string; kind: string };
type DagEdge = { id: string; source: string; target: string; label: string };
type Variant = { labels: string[]; nodes: DagNode[]; edges: DagEdge[] };
type DagResponse = { topic: string; fields: Record<string, Variant[]> };

// Mirrors the node-fill colors `src/bin/plot_dag.rs` uses for the DOT rendering, keyed on the
// `kind` string `osmnexus::dag::DagNode` stamps (see `src/dag.rs`).
const KIND_COLOR: Record<string, string> = {
  root: "#dbe9f6",
  match: "#e2f0d9",
  extract: "#d9e8fb",
  directed_extract: "#d9e8fb",
  const: "#d9e8fb",
  parent: "#fff2cc",
  sanitizer: "#f4cccc",
  step: "#ead1dc",
};

const NODE_W = 260;
const NODE_H = 90;
const GAP_X = 40;
const GAP_Y = 70;

// A `Producer`/`Sanitizer` tree from `src/dag.rs` is a strict tree (single root, each non-root node
// has exactly one incoming edge) — no need for a general graph-layout library. Post-order DFS: a
// leaf claims the next free column, an internal node centers over its children's span.
function layoutTree(nodes: DagNode[], edges: DagEdge[]): Map<string, { x: number; y: number }> {
  const childrenOf = new Map<string, string[]>();
  for (const e of edges) {
    if (!childrenOf.has(e.source)) childrenOf.set(e.source, []);
    childrenOf.get(e.source)!.push(e.target);
  }
  const positions = new Map<string, { x: number; y: number }>();
  let nextX = 0;
  function place(id: string, depth: number): number {
    const children = childrenOf.get(id) ?? [];
    let x: number;
    if (children.length === 0) {
      x = nextX;
      nextX += NODE_W + GAP_X;
    } else {
      const childXs = children.map((c) => place(c, depth + 1));
      x = (childXs[0] + childXs[childXs.length - 1]) / 2;
    }
    positions.set(id, { x, y: depth * (NODE_H + GAP_Y) });
    return x;
  }
  if (nodes[0]) place(nodes[0].id, 0);
  return positions;
}

// Plots the `Producer` tree behind a topic's output field — one tree per field, per distinct
// producer variant (categories sharing the same effective producer for that field collapse into
// one variant; see `src/bin/dag_json.rs`). Fetches fresh on every `topic` change since the tree
// reflects whatever's currently on disk in the (possibly just-edited) config.
export default function DagView({ topic }: { topic: string }) {
  const [response, setResponse] = useState<DagResponse | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [field, setField] = useState<string>("");
  const [variantIdx, setVariantIdx] = useState(0);

  useEffect(() => {
    setResponse(null);
    setError(null);
    if (!topic) return;
    fetch(`/api/dag/${encodeURIComponent(topic)}`)
      .then((r) => r.json())
      .then((d: DagResponse | { error: string }) => {
        if ("error" in d) {
          setError(d.error);
          return;
        }
        setResponse(d);
        setField((prev) => (d.fields[prev] ? prev : Object.keys(d.fields).sort()[0] ?? ""));
        setVariantIdx(0);
      })
      .catch((err) => setError(String(err)));
  }, [topic]);

  const fieldNames = useMemo(() => (response ? Object.keys(response.fields).sort() : []), [response]);
  const variants = response?.fields[field] ?? [];
  const variant = variants[variantIdx];

  const { nodes, edges } = useMemo(() => {
    if (!variant) return { nodes: [] as Node[], edges: [] as Edge[] };
    const positions = layoutTree(variant.nodes, variant.edges);
    const nodes: Node[] = variant.nodes.map((n) => ({
      id: n.id,
      position: positions.get(n.id) ?? { x: 0, y: 0 },
      data: { label: n.label },
      style: {
        background: KIND_COLOR[n.kind] ?? "#eee",
        border: "1px solid var(--border)",
        borderRadius: 6,
        padding: 8,
        fontSize: 11,
        fontFamily: "var(--font-mono)",
        whiteSpace: "pre-wrap",
        width: NODE_W,
      },
    }));
    const edges: Edge[] = variant.edges.map((e) => ({
      id: e.id,
      source: e.source,
      target: e.target,
      label: e.label || undefined,
      style: { stroke: "var(--border)" },
    }));
    return { nodes, edges };
  }, [variant]);

  if (error) {
    return (
      <div style={{ padding: 12, fontFamily: "var(--font-mono)", fontSize: 12, color: "var(--danger-text)" }}>
        {error}
      </div>
    );
  }
  if (!response) {
    return (
      <div style={{ padding: 12, fontFamily: "var(--font-ui)", fontSize: 12, color: "var(--muted)" }}>
        Loading…
      </div>
    );
  }

  return (
    <div style={{ display: "flex", flexDirection: "column", height: "100%", minHeight: 0 }}>
      <div style={{ display: "flex", gap: 8, padding: "6px 12px", borderBottom: "1px solid var(--border)", alignItems: "center" }}>
        <span style={{ fontFamily: "var(--font-ui)", fontSize: 12, color: "var(--muted)" }}>field</span>
        <select
          value={field}
          onChange={(e) => {
            setField(e.target.value);
            setVariantIdx(0);
          }}
          style={{ flex: 1, minWidth: 0, fontFamily: "var(--font-mono)", fontSize: 12 }}
        >
          {fieldNames.map((f) => (
            <option key={f} value={f}>
              {f}
              {response.fields[f].length > 1 ? ` (${response.fields[f].length} variants)` : ""}
            </option>
          ))}
        </select>
        {variants.length > 1 && (
          <select
            value={variantIdx}
            onChange={(e) => setVariantIdx(Number(e.target.value))}
            style={{ fontFamily: "var(--font-mono)", fontSize: 12 }}
          >
            {variants.map((v, i) => (
              <option key={i} value={i}>
                {v.labels.length > 3 ? `${v.labels.slice(0, 3).join(", ")}, …` : v.labels.join(", ")}
              </option>
            ))}
          </select>
        )}
      </div>
      <div style={{ flex: 1, minHeight: 0 }}>
        <ReactFlow nodes={nodes} edges={edges} fitView nodesDraggable={false} nodesConnectable={false} elementsSelectable={false}>
          <Background />
          <Controls />
        </ReactFlow>
      </div>
    </div>
  );
}
