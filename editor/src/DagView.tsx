import { useEffect, useMemo, useState } from "react";
import { ReactFlow, Background, BackgroundVariant, Controls, MarkerType, type Node, type Edge } from "@xyflow/react";
import "@xyflow/react/dist/style.css";

type DagNode = { id: string; label: string; kind: string };
type DagEdge = { id: string; source: string; target: string; label: string };
type Variant = { labels: string[]; nodes: DagNode[]; edges: DagEdge[] };
type DagResponse = { topic: string; fields: Record<string, Variant[]> };

// Mirrors the node-fill colors `src/bin/plot_dag.rs` uses for the DOT rendering, keyed on the
// `kind` string `osmnexus::dag::DagNode` stamps (see `src/dag.rs`) — paired with a matching border
// tone so nodes read as soft, tinted cards rather than flat fills with a generic gray outline.
const KIND_COLOR: Record<string, { bg: string; border: string }> = {
  match: { bg: "#e9f5e0", border: "#a9d18e" },
  rule: { bg: "#fdf3d9", border: "#e6c66b" },
  extract: { bg: "#dcebfc", border: "#8fb8e8" },
  directed_extract: { bg: "#dcebfc", border: "#8fb8e8" },
  const: { bg: "#dcebfc", border: "#8fb8e8" },
  parent: { bg: "#fff2cc", border: "#e0c25a" },
  sanitizer: { bg: "#f8d7d7", border: "#e08a8a" },
  step: { bg: "#f0dbe8", border: "#d197bd" },
  annotate: { bg: "#f3f3f4", border: "#d4d6da" },
};

const NODE_W = 260;
const NODE_H = 90;
const GAP_X = 44;
const GAP_Y = 76;
const ANNOTATE_GAP_X = 24;

// A `Producer`/`Sanitizer` tree from `src/dag.rs` is a strict tree (single root, each non-root node
// has exactly one incoming edge) — no need for a general graph-layout library. Post-order DFS: a
// leaf claims the next free column, an internal node centers over its children's span.
//
// "annotate" nodes (src/dag.rs's `annotate_node`) are excluded from that DFS entirely — they're not
// a step in the value's build flow, just a side note on their owner — and instead placed directly
// beside it afterward, same row, one node-width to the right.
function layoutTree(nodes: DagNode[], edges: DagEdge[]): Map<string, { x: number; y: number }> {
  const kindOf = new Map(nodes.map((n) => [n.id, n.kind]));
  const treeEdges = edges.filter((e) => kindOf.get(e.target) !== "annotate");
  const annotateEdges = edges.filter((e) => kindOf.get(e.target) === "annotate");

  const childrenOf = new Map<string, string[]>();
  for (const e of treeEdges) {
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

  for (const e of annotateEdges) {
    const ownerPos = positions.get(e.source);
    if (ownerPos) positions.set(e.target, { x: ownerPos.x + NODE_W + ANNOTATE_GAP_X, y: ownerPos.y });
  }
  return positions;
}

// Node count of a field's biggest variant — used to sort the field picker so the most interesting
// (most-branching) trees sort first instead of alphabetically.
function maxNodeCount(variants: Variant[]): number {
  return variants.reduce((max, v) => Math.max(max, v.nodes.length), 0);
}

// Plots the `Producer` tree behind a topic's output field (`mode: "deriver"`) or the categorization
// tree that assigns an object to a category in the first place (`mode: "category"`, one tree per
// `ElementKind` instead of per field; see `src/dag.rs`'s `category_order_dag`) — one tree per
// field/kind, per distinct variant (categories sharing the same effective producer for a field
// collapse into one variant; see `src/bin/dag_json.rs`). Fetches fresh on every `topic`/`mode`
// change since the tree reflects whatever's currently on disk in the (possibly just-edited) config.
export default function DagView({
  topic,
  category,
  mode = "deriver",
}: {
  topic: string;
  category?: string | null;
  mode?: "deriver" | "category";
}) {
  const [response, setResponse] = useState<DagResponse | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [field, setField] = useState<string>("");
  const [variantIdx, setVariantIdx] = useState(0);
  const fieldLabel = mode === "category" ? "kind" : "field";

  useEffect(() => {
    setResponse(null);
    setError(null);
    setField("");
    setVariantIdx(0);
    if (!topic) return;
    const endpoint = mode === "category" ? "/api/categorize-dag/" : "/api/dag/";
    fetch(`${endpoint}${encodeURIComponent(topic)}`)
      .then((r) => r.json())
      .then((d: DagResponse | { error: string }) => {
        if ("error" in d) {
          setError(d.error);
          return;
        }
        setResponse(d);
        const sortedFields = Object.keys(d.fields).sort((a, b) => maxNodeCount(d.fields[b]) - maxNodeCount(d.fields[a]));
        setField((prev) => (d.fields[prev] ? prev : sortedFields[0] ?? ""));
        setVariantIdx(0);
      })
      .catch((err) => setError(String(err)));
  }, [topic, mode]);

  const fieldNames = useMemo(
    () => (response ? Object.keys(response.fields).sort((a, b) => maxNodeCount(response.fields[b]) - maxNodeCount(response.fields[a])) : []),
    [response],
  );
  const variants = response?.fields[field] ?? [];
  const variant = variants[variantIdx];

  // A focused category (from the sidebar tree) limits which variant this view shows — jump to
  // whichever variant covers that category instead of leaving the last-picked one selected.
  useEffect(() => {
    if (!category || variants.length <= 1) return;
    const idx = variants.findIndex((v) => v.labels.includes(category));
    if (idx > 0) setVariantIdx(idx);
  }, [category, variants]);

  const { nodes, edges } = useMemo(() => {
    if (!variant) return { nodes: [] as Node[], edges: [] as Edge[] };
    const positions = layoutTree(variant.nodes, variant.edges);
    const nodes: Node[] = variant.nodes.map((n) => {
      const colors = KIND_COLOR[n.kind] ?? { bg: "#eee", border: "var(--border)" };
      return {
        id: n.id,
        position: positions.get(n.id) ?? { x: 0, y: 0 },
        data: { label: n.label },
        style: {
          background: colors.bg,
          border: n.kind === "annotate" ? `1.5px dashed ${colors.border}` : `1.5px solid ${colors.border}`,
          borderRadius: 10,
          padding: "10px 12px",
          fontSize: 12.5,
          fontFamily: "var(--font-mono)",
          whiteSpace: "pre-wrap",
          width: NODE_W,
          boxShadow: "0 1px 2px rgba(16, 24, 40, 0.06), 0 2px 6px rgba(16, 24, 40, 0.05)",
        },
      };
    });
    const edges: Edge[] = variant.edges.map((e) => ({
      id: e.id,
      source: e.source,
      target: e.target,
      label: e.label || undefined,
      type: "smoothstep",
      style: { stroke: "#9aa1ac", strokeWidth: 1.5 },
      labelStyle: { fontFamily: "var(--font-ui)", fontSize: 11, fill: "var(--muted)" },
      labelBgStyle: { fill: "var(--panel)" },
      labelBgPadding: [4, 2] as [number, number],
      labelBgBorderRadius: 4,
      markerEnd: { type: MarkerType.ArrowClosed, width: 16, height: 16, color: "#9aa1ac" },
    }));
    return { nodes, edges };
  }, [variant]);

  if (error) {
    return (
      <div style={{ padding: 14, fontFamily: "var(--font-mono)", fontSize: 13, color: "var(--danger-text)" }}>
        {error}
      </div>
    );
  }
  if (!response) {
    return (
      <div style={{ padding: 14, fontFamily: "var(--font-ui)", fontSize: 13, color: "var(--muted)" }}>
        Loading…
      </div>
    );
  }

  return (
    <div style={{ display: "flex", flexDirection: "column", height: "100%", minHeight: 0 }}>
      <div
        style={{
          display: "flex",
          gap: 10,
          padding: "10px 14px",
          borderBottom: "1px solid var(--border)",
          alignItems: "center",
          background: "var(--panel)",
          boxShadow: "var(--shadow-sm)",
        }}
      >
        <span style={{ fontFamily: "var(--font-ui)", fontSize: 12, fontWeight: 600, color: "var(--muted)" }}>{fieldLabel}</span>
        <select
          value={field}
          onChange={(e) => {
            setField(e.target.value);
            setVariantIdx(0);
          }}
          style={{ flex: 1, minWidth: 0, fontFamily: "var(--font-mono)", fontSize: 13 }}
        >
          {fieldNames.map((f) => (
            <option key={f} value={f}>
              {f} ({maxNodeCount(response.fields[f])} nodes
              {response.fields[f].length > 1 ? `, ${response.fields[f].length} variants` : ""})
            </option>
          ))}
        </select>
        {variants.length > 1 && (
          <select
            value={variantIdx}
            onChange={(e) => setVariantIdx(Number(e.target.value))}
            style={{ fontFamily: "var(--font-mono)", fontSize: 13 }}
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
        <ReactFlow
          key={`${field}-${variantIdx}`}
          nodes={nodes}
          edges={edges}
          fitView
          fitViewOptions={{ padding: 0.15 }}
          nodesDraggable={false}
          nodesConnectable={false}
          elementsSelectable={false}
          proOptions={{ hideAttribution: true }}
        >
          <Background variant={BackgroundVariant.Dots} gap={20} size={1.5} color="#d8dbe0" />
          <Controls showInteractive={false} />
        </ReactFlow>
      </div>
    </div>
  );
}
