"use client";

import { useEffect, useMemo, useState, type ReactNode } from "react";
import {
  ReactFlow,
  Background,
  BackgroundVariant,
  BaseEdge,
  Controls,
  EdgeLabelRenderer,
  Handle,
  MarkerType,
  Position,
  getSmoothStepPath,
  type Node,
  type Edge,
  type EdgeProps,
  type NodeProps,
} from "@xyflow/react";
import "@xyflow/react/dist/style.css";

type DagNode = { id: string; label: string; kind: string };
type DagEdge = { id: string; source: string; target: string; label: string };
type Variant = { labels: string[]; nodes: DagNode[]; edges: DagEdge[] };
type ListResponse = { topic: string; names: string[] };
type GraphResponse = { topic: string; name: string; variants: Variant[] };

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

// Every node gets four handles: the default top/bottom pair for the tree's normal parent-child
// flow, plus a right/left pair used only by annotate edges (an annotate node sits beside its owner,
// same row, not below it — routing that edge through the default top/bottom handles is what drew
// the Z-shaped line: down out of the owner's bottom, sideways, then up into the annotate node's top).
function DagNodeBox({ data }: NodeProps) {
  const hidden = { opacity: 0 } as const;
  // First line is the node's type (e.g. "Extract", "Mapping") — centered, since it's a heading, not
  // a value. Remaining lines are that type's own arguments (key/value pairs, counts) — left-bound.
  const [typeLine, ...argLines] = (data.label as string).split("\n");
  return (
    <>
      <Handle type="target" position={Position.Top} style={hidden} />
      <Handle type="source" position={Position.Bottom} style={hidden} />
      <Handle type="target" position={Position.Left} id="left" style={hidden} />
      <Handle type="source" position={Position.Right} id="right" style={hidden} />
      <div style={{ textAlign: "center", fontWeight: 600 }}>{typeLine}</div>
      {argLines.length > 0 && (
        <div style={{ textAlign: "left", marginTop: 4 }}>{argLines.join("\n")}</div>
      )}
    </>
  );
}

const NODE_TYPES = { dagNode: DagNodeBox };

// Sibling edges (e.g. every "branch\ntag: X" edge fanning out of a decision-tree branch node) all
// leave their source from the same handle, so their paths are coincident right where they exit —
// the default per-edge label (drawn inline in that edge's own SVG group) could end up painted over
// by a later sibling's line there, since SVG paint order just follows the edges array. Rendering the
// label through `EdgeLabelRenderer` instead puts it in one shared DOM layer that sits above the
// entire edges SVG pane, so no edge's line can ever cover another edge's label.
function DagEdge({ id, sourceX, sourceY, targetX, targetY, sourcePosition, targetPosition, label, style, markerEnd }: EdgeProps) {
  const [edgePath, labelX, labelY] = getSmoothStepPath({ sourceX, sourceY, sourcePosition, targetX, targetY, targetPosition });
  return (
    <>
      <BaseEdge id={id} path={edgePath} style={style} markerEnd={markerEnd} />
      {label != null && (
        <EdgeLabelRenderer>
          <div
            style={{
              position: "absolute",
              transform: `translate(-50%, -50%) translate(${labelX}px, ${labelY}px)`,
              background: "var(--panel)",
              padding: "2px 4px",
              borderRadius: 4,
              fontFamily: "var(--font-ui)",
              fontSize: 11,
              color: "var(--muted)",
              pointerEvents: "none",
            }}
          >
            {label}
          </div>
        </EdgeLabelRenderer>
      )}
    </>
  );
}

const EDGE_TYPES = { dagEdge: DagEdge };

const NODE_W = 260;
const NODE_W_MAX = 480;
const NODE_H = 90;
const GAP_X = 70;
const GAP_Y = 110;
const ANNOTATE_GAP_X = 40;
// Rough monospace advance at the node's 12.5px font, plus the box's horizontal padding — used to
// widen a node past NODE_W when its longest line (e.g. a decision-tree leaf's list of candidate
// category names) wouldn't otherwise fit.
const CHAR_W = 7.4;
const NODE_PAD_X = 24;
// Rough line height at the node's 12.5px font, plus the box's vertical padding (`10px 12px` in the
// node style below) — used to grow a node past NODE_H when it has more argument lines than a
// typical node (e.g. a decision-tree leaf listing many candidate category names, or a `Mapping`
// step with several entries), the same way `widthFor` grows width past NODE_W.
const LINE_H = 17;
const NODE_PAD_Y = 24;

function widthFor(label: string): number {
  const maxLine = Math.max(...label.split("\n").map((l) => l.length));
  return Math.min(NODE_W_MAX, Math.max(NODE_W, Math.ceil(maxLine * CHAR_W) + NODE_PAD_X));
}

function heightFor(label: string): number {
  const lines = label.split("\n").length;
  return Math.max(NODE_H, lines * LINE_H + NODE_PAD_Y);
}

// A `Producer`/`Sanitizer` tree from `src/dag.rs` is a strict tree (single root, each non-root node
// has exactly one incoming edge) — no need for a general graph-layout library. Post-order DFS: a
// leaf claims the next free column, an internal node centers over its children's span.
//
// "annotate" nodes (src/dag.rs's `annotate_node`) are excluded from that DFS entirely — they're not
// a step in the value's build flow, just a side note on their owner — and instead placed directly
// beside it afterward, same row, one node-width to the right.
function layoutTree(
  nodes: DagNode[],
  edges: DagEdge[],
): { positions: Map<string, { x: number; y: number }>; widths: Map<string, number>; heights: Map<string, number> } {
  const kindOf = new Map(nodes.map((n) => [n.id, n.kind]));
  const widths = new Map(nodes.map((n) => [n.id, widthFor(n.label)]));
  const heights = new Map(nodes.map((n) => [n.id, heightFor(n.label)]));
  const treeEdges = edges.filter((e) => kindOf.get(e.target) !== "annotate");
  const annotateEdges = edges.filter((e) => kindOf.get(e.target) === "annotate");

  const childrenOf = new Map<string, string[]>();
  for (const e of treeEdges) {
    if (!childrenOf.has(e.source)) childrenOf.set(e.source, []);
    childrenOf.get(e.source)!.push(e.target);
  }
  const depthOf = new Map<string, number>();
  const positions = new Map<string, { x: number; y: number }>();
  let nextX = 0;
  // Returns each placed node's left edge and right edge (left + its own width), so a parent centers
  // over the true span of its children instead of just the average of their left edges. Only `x` is
  // final here — `y` depends on every other node's height at the same depth (a tall sibling several
  // branches over still pushes this row's children down), so that's a second pass below once every
  // node's depth is known.
  function place(id: string, depth: number): [left: number, right: number] {
    depthOf.set(id, depth);
    const width = widths.get(id) ?? NODE_W;
    const children = childrenOf.get(id) ?? [];
    let left: number;
    if (children.length === 0) {
      left = nextX;
      nextX += width + GAP_X;
    } else {
      const spans = children.map((c) => place(c, depth + 1));
      const spanLeft = spans[0][0];
      const spanRight = spans[spans.length - 1][1];
      left = (spanLeft + spanRight) / 2 - width / 2;
    }
    positions.set(id, { x: left, y: 0 });
    return [left, left + width];
  }
  // The tree's root is whichever node is never a `treeEdges` target — not necessarily `nodes[0]`:
  // `src/dag.rs`'s `render_chain` creates an `Extract` leaf's own node before the sanitize steps
  // that feed into it, so for a field whose top-level producer is a sanitized `Extract`, the first
  // node in the array is that leaf, not the chain's actual entry point.
  const hasIncoming = new Set(treeEdges.map((e) => e.target));
  const root = nodes.find((n) => !hasIncoming.has(n.id)) ?? nodes[0];
  if (root) place(root.id, 0);

  // Each row's height is its tallest node's — a multi-line leaf (e.g. a decision-tree leaf's long
  // candidate list) otherwise overflows NODE_H's fixed row spacing and overlaps the row below it.
  const rowHeight = new Map<number, number>();
  for (const [id, depth] of depthOf) {
    rowHeight.set(depth, Math.max(rowHeight.get(depth) ?? 0, heights.get(id) ?? NODE_H));
  }
  const rowY = new Map<number, number>();
  let y = 0;
  for (let d = 0; d <= Math.max(0, ...rowHeight.keys()); d++) {
    rowY.set(d, y);
    y += (rowHeight.get(d) ?? NODE_H) + GAP_Y;
  }
  for (const [id, depth] of depthOf) {
    const p = positions.get(id)!;
    positions.set(id, { x: p.x, y: rowY.get(depth) ?? 0 });
  }

  for (const e of annotateEdges) {
    const ownerPos = positions.get(e.source);
    const ownerWidth = widths.get(e.source) ?? NODE_W;
    if (ownerPos) positions.set(e.target, { x: ownerPos.x + ownerWidth + ANNOTATE_GAP_X, y: ownerPos.y });
  }
  return { positions, widths, heights };
}

// Plots the `Producer` tree behind a topic's output field (`mode: "deriver"`), or, per `ElementKind`
// instead of per field, either its category picker (`mode: "category"` — one category's own
// condition + what it excludes at a time, picked from its kind's priority order; see `src/dag.rs`'s
// `category_condition_dag`) or its compiled discrimination net (`mode: "decision-tree"`). Fetches
// are staged so `dag_json` only ever builds the one graph actually on screen rather than every
// field's/category's, which was the dominant load-time cost for topics with many/large ones: the
// field/kind name list (on every `topic`/`mode` change), then, for `category` mode only, that kind's
// category names in priority order, then finally the one graph actually selected.
export default function DagView({
  topic,
  category,
  mode = "deriver",
  extraHeader,
}: {
  topic: string;
  category?: string | null;
  mode?: "deriver" | "category" | "decision-tree";
  // Rendered at the right end of this view's own header row (its field/category/variant dropdowns
  // sit at the left) — App.tsx uses this for the Map/Tree/Categorize/Decision-tree view switcher, so
  // it's part of this row's normal flow instead of an absolute overlay that used to sit on top of
  // (and get overlapped by) whichever dropdowns ran the full row width.
  extraHeader?: ReactNode;
}) {
  const [fieldNames, setFieldNames] = useState<string[] | null>(null);
  const [variants, setVariants] = useState<Variant[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [field, setField] = useState<string>("");
  const [variantIdx, setVariantIdx] = useState(0);
  // `category` mode has a third level: `field` here is the *kind* (way/node/relation), and within
  // it a category is picked separately — one tree per category (its own condition + what it
  // excludes) instead of cramming every category for a kind into one "try these in order" tree, per
  // `src/dag.rs`'s `category_condition_dag`. Names arrive already in the kind's priority order
  // (`order`, the actual runtime first-match sequence) — never re-sorted, unlike `fieldNames`.
  const [categoryNames, setCategoryNames] = useState<string[] | null>(null);
  const [categoryIdx, setCategoryIdx] = useState(0);
  const fieldLabel = mode === "deriver" ? "field" : "kind";
  const endpoint = mode === "category" ? "/api/categorize-dag/" : mode === "decision-tree" ? "/api/decision-tree-dag/" : "/api/dag/";

  useEffect(() => {
    let ignore = false;
    setFieldNames(null);
    setVariants([]);
    setCategoryNames(null);
    setCategoryIdx(0);
    setError(null);
    setField("");
    setVariantIdx(0);
    if (!topic) return;
    fetch(`${endpoint}${encodeURIComponent(topic)}`)
      .then((r) => r.json())
      .then((d: ListResponse | { error: string }) => {
        if (ignore) return;
        if ("error" in d) {
          setError(d.error);
          return;
        }
        const sortedNames = [...d.names].sort();
        setFieldNames(sortedNames);
        setField(sortedNames[0] ?? "");
      })
      .catch((err) => !ignore && setError(String(err)));
    return () => {
      ignore = true;
    };
  }, [topic, mode, endpoint]);

  // `category` mode only: once a kind is confirmed selected, fetch its category names — in
  // priority order, so the picker matches the actual first-match evaluation sequence.
  useEffect(() => {
    let ignore = false;
    setCategoryNames(null);
    setCategoryIdx(0);
    if (mode !== "category" || !topic || !field || !fieldNames?.includes(field)) return;
    fetch(`${endpoint}${encodeURIComponent(topic)}?name=${encodeURIComponent(field)}`)
      .then((r) => r.json())
      .then((d: ListResponse | { error: string }) => {
        if (ignore) return;
        if ("error" in d) {
          setError(d.error);
          return;
        }
        setCategoryNames(d.names);
      })
      .catch((err) => !ignore && setError(String(err)));
    return () => {
      ignore = true;
    };
  }, [topic, mode, endpoint, field, fieldNames]);

  useEffect(() => {
    let ignore = false;
    setVariants([]);
    setVariantIdx(0);
    // `field` can briefly still hold the previous mode's selection here — the effect above resets
    // it, but on a `mode` change both effects fire in the same pass, before that reset has
    // committed. Skip rather than fire a request doomed to 404/error against the new mode/endpoint;
    // the field-list effect will set a real field for this mode shortly and re-trigger this one.
    if (!topic || !field || !fieldNames?.includes(field)) return;
    const url =
      mode === "category"
        ? categoryNames && categoryNames.length > 0
          ? `${endpoint}${encodeURIComponent(topic)}?name=${encodeURIComponent(field)}&idx=${categoryIdx}`
          : null
        : `${endpoint}${encodeURIComponent(topic)}?name=${encodeURIComponent(field)}`;
    if (!url) return;
    console.log("DEBUG fetching", url);
    fetch(url)
      .then((r) => r.json())
      .then((d: GraphResponse | { error: string }) => {
        console.log("DEBUG fetched", ignore, "error" in d, !("error" in d) && d.variants?.length);
        if (ignore) return;
        if ("error" in d) {
          setError(d.error);
          return;
        }
        setVariants(d.variants);
      })
      .catch((err) => !ignore && setError(String(err)));
    return () => {
      ignore = true;
    };
  }, [topic, mode, endpoint, field, fieldNames, categoryNames, categoryIdx]);

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
    const { positions, widths } = layoutTree(variant.nodes, variant.edges);
    const nodes: Node[] = variant.nodes.map((n) => {
      const colors = KIND_COLOR[n.kind] ?? { bg: "#eee", border: "var(--border)" };
      return {
        id: n.id,
        type: "dagNode",
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
          width: widths.get(n.id) ?? NODE_W,
          boxShadow: "0 1px 2px rgba(16, 24, 40, 0.06), 0 2px 6px rgba(16, 24, 40, 0.05)",
        },
      };
    });
    const kindOf = new Map(variant.nodes.map((n) => [n.id, n.kind]));
    const edges: Edge[] = variant.edges.map((e) => ({
      id: e.id,
      source: e.source,
      target: e.target,
      ...(kindOf.get(e.target) === "annotate" ? { sourceHandle: "right", targetHandle: "left" } : {}),
      label: e.label || undefined,
      type: "dagEdge",
      style: { stroke: "#9aa1ac", strokeWidth: 1.5 },
      markerEnd: { type: MarkerType.ArrowClosed, width: 16, height: 16, color: "#9aa1ac" },
    }));
    return { nodes, edges };
  }, [variant]);

  const header = (content: ReactNode) => (
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
        {content}
        {extraHeader && <div style={{ marginLeft: "auto", display: "flex", gap: 8 }}>{extraHeader}</div>}
      </div>
      <div style={{ flex: 1, minHeight: 0, overflow: "auto" }} />
    </div>
  );

  if (error) {
    return header(
      <span style={{ fontFamily: "var(--font-mono)", fontSize: 13, color: "var(--danger-text)" }}>{error}</span>,
    );
  }
  if (!fieldNames) {
    return header(
      <span style={{ fontFamily: "var(--font-ui)", fontSize: 13, color: "var(--muted)" }}>Loading…</span>,
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
              {f}
            </option>
          ))}
        </select>
        {mode === "category" && categoryNames && (
          <>
            <span style={{ fontFamily: "var(--font-ui)", fontSize: 12, fontWeight: 600, color: "var(--muted)" }}>category</span>
            <select
              value={categoryIdx}
              onChange={(e) => setCategoryIdx(Number(e.target.value))}
              style={{ flex: 1, minWidth: 0, fontFamily: "var(--font-mono)", fontSize: 13 }}
            >
              {categoryNames.map((c, i) => (
                <option key={i} value={i}>
                  {c}
                </option>
              ))}
            </select>
          </>
        )}
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
        {extraHeader && <div style={{ marginLeft: "auto", display: "flex", gap: 8 }}>{extraHeader}</div>}
      </div>
      <div style={{ flex: 1, minHeight: 0 }}>
        <ReactFlow
          // `fitView` only fits on mount — the graph now arrives via its own fetch, separate from
          // (and later than) the field/variant selection, so without the `nodes.length > 0` flip in
          // the key, this instance would already be mounted (empty) by the time the real nodes show
          // up and never re-fit to them.
          key={`${field}-${categoryIdx}-${variantIdx}-${nodes.length > 0}`}
          nodeTypes={NODE_TYPES}
          edgeTypes={EDGE_TYPES}
          nodes={nodes}
          edges={edges}
          fitView
          // A wide, shallow tree (a decision-tree branch node fanning out into dozens of same-depth
          // leaves is the typical case) can be dozens of node-widths wide but only a couple tall — fit
          // that at NODE_W's true scale and the required zoom is a fraction of `minZoom`'s own
          // floor, so `fitView` shrinks the whole thing into an unreadable sliver rather than ever
          // hitting that floor. Capping fitView's own zoom range (separate from `minZoom`, which
          // still lets a user manually zoom out further if they want the full-graph overview) keeps
          // the initial view at a legible scale — a centered slice instead of the whole illegible
          // thing.
          fitViewOptions={{ padding: 0.15, minZoom: 0.3 }}
          minZoom={0.05}
          maxZoom={2}
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
