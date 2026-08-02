"use client";

import { useEffect, useMemo, useRef, useState } from "react";
import Map from "./Map";
import Editor from "./Editor";
import DagView from "./DagView";

const DEFAULT_KIND = "way";
const DEBOUNCE_MS = 300;
const NEW_CATEGORY_JSON = '{"condition":{}}';
const MIN_BBOX_M = 100;
const DEFAULT_MAX_BBOX_M = 10000; // Used until /api/bounds' server-configured value (MAX_BBOX_M env var) loads.
const METERS_PER_DEG_LAT = 111_320;

// Approximate bbox extents in meters (equirectangular — fine at this scale/precision, no need for
// a real geodesic library just to sanity-check a manually-dragged selection size).
function bboxSizeMeters(box: [number, number, number, number]): { widthM: number; heightM: number } {
  const [west, south, east, north] = box;
  const midLatRad = ((south + north) / 2) * (Math.PI / 180);
  return {
    widthM: (east - west) * METERS_PER_DEG_LAT * Math.cos(midLatRad),
    heightM: (north - south) * METERS_PER_DEG_LAT,
  };
}

// Drag-resizes one numeric size (px) along one axis, from a divider's `onMouseDown`. `current` is
// the size at drag-start (captured once, not re-read mid-drag, since `set` triggers a re-render but
// this closure keeps running against the value it started with — dragging is relative to where the
// drag began, not to whatever the size happens to be after intermediate re-renders). `invert` flips
// which direction growth is: a divider left of the panel it resizes needs dragging *left* (negative
// delta) to grow it.
function beginResize(e: React.MouseEvent, axis: "x" | "y", current: number, set: (n: number) => void, min: number, max: number, invert = false) {
  e.preventDefault();
  const start = axis === "x" ? e.clientX : e.clientY;
  const prevUserSelect = document.body.style.userSelect;
  document.body.style.userSelect = "none";
  const onMove = (ev: MouseEvent) => {
    const cur = axis === "x" ? ev.clientX : ev.clientY;
    const delta = (cur - start) * (invert ? -1 : 1);
    set(Math.min(max, Math.max(min, current + delta)));
  };
  const onUp = () => {
    document.body.style.userSelect = prevUserSelect;
    window.removeEventListener("mousemove", onMove);
    window.removeEventListener("mouseup", onUp);
  };
  window.addEventListener("mousemove", onMove);
  window.addEventListener("mouseup", onUp);
}

// A thin drag handle between two panels — transparent until hovered/dragged, so it doesn't add
// visual clutter to a UI that otherwise has none.
function Divider({ axis, onMouseDown }: { axis: "x" | "y"; onMouseDown: (e: React.MouseEvent) => void }) {
  const [highlight, setHighlight] = useState(false);
  return (
    <div
      onMouseEnter={() => setHighlight(true)}
      onMouseLeave={() => setHighlight(false)}
      onMouseDown={(e) => {
        setHighlight(true);
        onMouseDown(e);
        const onUp = () => {
          setHighlight(false);
          window.removeEventListener("mouseup", onUp);
        };
        window.addEventListener("mouseup", onUp);
      }}
      style={{
        flexShrink: 0,
        cursor: axis === "x" ? "col-resize" : "row-resize",
        background: highlight ? "var(--accent, #6b8afd)" : "transparent",
        ...(axis === "x" ? { width: 5, marginLeft: -2.5, marginRight: -2.5 } : { height: 5, marginTop: -2.5, marginBottom: -2.5 }),
        zIndex: 1,
      }}
    />
  );
}

type Category = { topic: string; kind: string; name: string };
// A "selection" is either a category (kind/name point at a way/node/relation category file) or the
// topic's own topic.json (table/exclude_condition/osm_fields) — same editor pane, different file.
type Selection = Category & { isTopicConfig?: boolean };
const NO_SELECTION: Selection = { topic: "", kind: DEFAULT_KIND, name: "" };

// Deterministic (not reshuffled every render) but effectively-random hue per key, so each
// topic/category pair gets a stable color on the map without needing a maintained palette.
// Keyed on topic+category (not category alone) since category *names* collide across topics —
// e.g. osmnx's bike/walk/drive topics all use a single category literally named "all".
function hashColor(key: string): string {
  let hash = 0;
  for (let i = 0; i < key.length; i++) hash = (hash * 31 + key.charCodeAt(i)) | 0;
  const hue = Math.abs(hash) % 360;
  return `hsl(${hue}, 70%, 55%)`;
}

export default function App() {
  const [bounds, setBounds] = useState<[number, number, number, number] | null>(null);
  const [selected, setSelected] = useState(false);
  const [maxBboxM, setMaxBboxM] = useState(DEFAULT_MAX_BBOX_M);
  const [extracting, setExtracting] = useState(false);
  const [configs, setConfigs] = useState<string[]>([]);
  const [currentConfig, setCurrentConfig] = useState("");
  const [topics, setTopics] = useState<string[]>([]);
  const [hiddenTopics, setHiddenTopics] = useState<Set<string>>(new Set());
  const [expandedTopics, setExpandedTopics] = useState<Set<string>>(new Set());
  const [categoriesByTopic, setCategoriesByTopic] = useState<Record<string, Category[]>>({});
  const [active, setActive] = useState<Selection>(NO_SELECTION);
  // Bumped on every category click (see Map's `focusTick` doc comment) — separate from `active`
  // itself since re-clicking the already-active category doesn't change `active.topic`/`.name`,
  // but should still refocus the map on it.
  const [focusTick, setFocusTick] = useState(0);
  // True only once the user has explicitly clicked a category in the sidebar — `active` also gets
  // set by auto-select (first category on mount, see loadCategories) purely so the editor pane has
  // something to show, but that shouldn't isolate the map down to a single category: the
  // alphabetically-first category can easily have zero matches in the current bbox (e.g. a rare
  // subtype), which made the map look broken — lines vanish (isolated to an empty category) while
  // cut points, which aren't category-scoped, keep showing. Gating `isolateCategory` on this instead
  // of `active` means the map shows every category's features until the user actually asks to focus.
  const [manualSelect, setManualSelect] = useState(false);
  const [newNameByTopic, setNewNameByTopic] = useState<Record<string, string>>({});
  const [collapsed, setCollapsed] = useState(false);
  // Drag-resizable panel sizes (px) — the map/tree pane vs. the side panel, and, within the side
  // panel, the topics/category list vs. the JSON editor below it. Defaults roughly match the old
  // fixed 60/40 flex split and the old 320px list `maxHeight`.
  const [sidebarWidth, setSidebarWidth] = useState(420);
  const [topicsListHeight, setTopicsListHeight] = useState(320);
  const [showNodes, setShowNodes] = useState(false);
  const [followSelection, setFollowSelection] = useState(true);
  const [viewMode, setViewMode] = useState<"map" | "tree" | "categorize" | "decision-tree">("map");
  const [text, setText] = useState<string>("");
  const [data, setData] = useState<GeoJSON.FeatureCollection | null>(null);
  const [cutPoints, setCutPoints] = useState<GeoJSON.FeatureCollection | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [extractMs, setExtractMs] = useState<number | null>(null);
  const [pipelineMs, setPipelineMs] = useState<number | null>(null);
  const debounceRef = useRef<ReturnType<typeof setTimeout> | null>(null);

  useEffect(() => {
    fetch("/api/bounds")
      .then((r) => r.json())
      .then((d) => {
        setBounds(d.bounds);
        setSelected(d.selected);
        if (d.maxBboxM) setMaxBboxM(d.maxBboxM);
      });
    fetch("/api/configs")
      .then((r) => r.json())
      .then((d: { configs: string[]; current: string }) => {
        setConfigs(d.configs);
        setCurrentConfig(d.current);
      });
    loadTopics();
  }, []);

  // Fetches the topics/categories for whichever config the server currently has selected —
  // shared by the initial mount and by switchConfig.
  function loadTopics() {
    fetch("/api/topics")
      .then((r) => r.json())
      .then((d: { topics: string[] }) => {
        setTopics(d.topics);
        // Topics start collapsed and nothing is selected — classify the first topic directly
        // (bypassing `active`/`text`) so the map shows data immediately without highlighting a row
        // or populating the editor pane, which would look selected despite nothing being expanded.
        if (d.topics[0]) loadInitialMap(d.topics[0]);
        for (const topic of d.topics) loadCategories(topic);
      });
  }

  async function loadInitialMap(topic: string) {
    try {
      const r = await fetch(`/api/topic/${encodeURIComponent(topic)}`);
      const d = await r.json();
      await classify(d.json, { topic, kind: "", name: "", isTopicConfig: true });
    } catch {
      // Ignore — the map just stays empty until the user picks something.
    }
  }

  function loadCategories(topic: string) {
    fetch(`/api/categories/${encodeURIComponent(topic)}`)
      .then((r) => r.json())
      .then((d: { categories: { kind: string; name: string }[] }) => {
        const cats = d.categories.map((c) => ({ topic, ...c }));
        setCategoriesByTopic((prev) => ({ ...prev, [topic]: cats }));
      });
  }

  // Switches the server's selected config directory, then resets every topic-scoped piece of
  // state (categories/selection differ entirely between configs) and reloads — same load path as
  // the initial mount, so the freshly switched config classifies its first topic right away (if a
  // bbox is already chosen) via loadTopics -> loadInitialMap. Keeps the current bbox and map data
  // as-is (no reset/reload) — the old config's features stay on screen until the new config's
  // classify call replaces them in place, instead of blanking the map while switching.
  async function switchConfig(config: string) {
    const res = await fetch("/api/config", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ config }),
    });
    if (!res.ok) {
      const body = await res.json().catch(() => ({}));
      setError(body.error || "Failed to switch config");
      return;
    }
    setCurrentConfig(config);
    setTopics([]);
    setCategoriesByTopic({});
    setHiddenTopics(new Set());
    setExpandedTopics(new Set());
    setNewNameByTopic({});
    setActive(NO_SELECTION);
    setManualSelect(false);
    loadTopics();
  }

  useEffect(() => {
    if (active.isTopicConfig) {
      fetch(`/api/topic/${encodeURIComponent(active.topic)}`)
        .then((r) => r.json())
        .then((d) => setText(d.json))
        .catch(() => setText("{}"));
      return;
    }
    if (!active.topic || !active.name) {
      setText(NEW_CATEGORY_JSON);
      return;
    }
    fetch(`/api/category/${encodeURIComponent(active.topic)}/${encodeURIComponent(active.kind)}/${encodeURIComponent(active.name)}`)
      .then((r) => r.json())
      .then((d) => setText(d.json))
      .catch(() => setText(NEW_CATEGORY_JSON));
  }, [active]);

  // Memoized so identity only changes when the selected topic/category actually does — an inline
  // object literal here would change reference on every render (e.g. every keystroke while
  // editing), which would defeat Map's focus effect's ability to tell "selection changed" from
  // "unrelated re-render".
  const isolateCategory = useMemo(
    () => (manualSelect && active.name && !active.isTopicConfig ? { topic: active.topic, name: active.name } : null),
    [manualSelect, active.topic, active.name, active.isTopicConfig],
  );

  // What the map should fit its view to on the next focusTick — a topic click fits every feature
  // in that topic (name: null); a category click narrows to just that category. Kept separate from
  // `isolateCategory` since a topic click shouldn't hide that topic's other categories on the map,
  // just move the viewport.
  const focusTarget = useMemo(
    () => (manualSelect && active.topic ? { topic: active.topic, name: active.isTopicConfig ? null : active.name || null } : null),
    [manualSelect, active.topic, active.name, active.isTopicConfig],
  );

  // One color per topic (not per category, for now) — categories within a topic aren't
  // individually distinguished yet.
  const topicColors = useMemo(() => {
    const colors: Record<string, string> = {};
    for (const topic of topics) colors[topic] = hashColor(topic);
    return colors;
  }, [topics]);

  async function addCategory(topic: string) {
    const name = (newNameByTopic[topic] ?? "").trim();
    if (!name) return;
    setNewNameByTopic((prev) => ({ ...prev, [topic]: "" }));
    const category = { topic, kind: DEFAULT_KIND, name };
    setActive(category);
    setText(NEW_CATEGORY_JSON);
    await classify(NEW_CATEGORY_JSON, category);
    loadCategories(topic);
  }

  async function selectBbox(box: [number, number, number, number]) {
    const { widthM, heightM } = bboxSizeMeters(box);
    if (widthM < MIN_BBOX_M || heightM < MIN_BBOX_M) {
      setError(`Selected area is too small (${Math.round(widthM)}m × ${Math.round(heightM)}m) — must be at least ${MIN_BBOX_M}m × ${MIN_BBOX_M}m.`);
      return;
    }
    if (widthM > maxBboxM || heightM > maxBboxM) {
      setError(`Selected area is too large (${Math.round(widthM)}m × ${Math.round(heightM)}m) — must be at most ${maxBboxM}m × ${maxBboxM}m.`);
      return;
    }
    setExtracting(true);
    setError(null);
    try {
      const res = await fetch("/api/extract", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ bounds: box }),
      });
      const body = await res.json();
      if (res.ok) {
        setBounds(body.bounds);
        setSelected(true);
        setExtractMs(body.extractMs ?? null);
        setData(null);
        if (text && active.topic && (active.name || active.isTopicConfig)) classify(text);
        else if (topics[0]) loadInitialMap(topics[0]);
      } else {
        setError(body.error || "Unknown error");
      }
    } finally {
      setExtracting(false);
    }
  }

  useEffect(() => {
    if (!text || !active.topic || (!active.name && !active.isTopicConfig)) return;
    if (debounceRef.current) clearTimeout(debounceRef.current);
    debounceRef.current = setTimeout(() => {
      classify(text);
    }, DEBOUNCE_MS);
    return () => {
      if (debounceRef.current) clearTimeout(debounceRef.current);
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [text, active]);

  async function deleteCategory(c: Category) {
    const res = await fetch(`/api/category/${encodeURIComponent(c.topic)}/${encodeURIComponent(c.kind)}/${encodeURIComponent(c.name)}`, { method: "DELETE" });
    if (!res.ok) {
      const body = await res.json().catch(() => ({}));
      setError(body.error || "Failed to delete category");
      return;
    }
    const remaining = (categoriesByTopic[c.topic] ?? []).filter((x) => !(x.kind === c.kind && x.name === c.name));
    setCategoriesByTopic((prev) => ({ ...prev, [c.topic]: remaining }));
    if (active.topic === c.topic && active.kind === c.kind && active.name === c.name) {
      setActive(remaining[0] ?? { topic: c.topic, kind: DEFAULT_KIND, name: "" });
    } else if (selected && active.topic && (active.name || active.isTopicConfig)) {
      classify(text);
    }
  }

  function toggleTopicVisibility(topic: string) {
    setHiddenTopics((prev) => {
      const next = new Set(prev);
      if (next.has(topic)) next.delete(topic);
      else next.add(topic);
      return next;
    });
  }

  function toggleTopicExpanded(topic: string) {
    setExpandedTopics((prev) => {
      const next = new Set(prev);
      if (next.has(topic)) next.delete(topic);
      else next.add(topic);
      // Folding every topic away clears the selection — reopening a (possibly different) topic
      // afterward should start from "nothing selected", not silently resurface whatever was open
      // before everything got minimized.
      if (next.size === 0) setActive(NO_SELECTION);
      return next;
    });
  }

  async function classify(json: string, selection: Selection = active) {
    try {
      JSON.parse(json);
    } catch (err) {
      setError(`Invalid JSON: ${(err as Error).message}`);
      return;
    }
    const res = selection.isTopicConfig
      ? await fetch(`/api/topic/${encodeURIComponent(selection.topic)}`, {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({ json }),
        })
      : await fetch("/api/classify", {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({ topic: selection.topic, kind: selection.kind, name: selection.name, json }),
        });
    const body = await res.json();
    if (res.ok) {
      setError(null);
      setData(body);
      setCutPoints(body.cutPoints ?? null);
      setPipelineMs(body.pipelineMs ?? null);
    } else {
      setError(body.error || "Unknown error");
    }
  }

  // Map/Tree/Categorize/Decision-tree switcher. Only the map view has no header row of its own to
  // put this in — it's a full-bleed canvas — so it floats as an absolute top-right overlay there.
  // The tree views (`DagView`) have their own header row full of field/category/variant dropdowns,
  // so this gets passed into that row instead (`extraHeader`, pushed right via `margin-left: auto`)
  // rather than absolutely overlaid on top of it, which is what used to cover up (or get covered
  // by, depending on which dropdown happened to be wide that render) whatever sat at that row's
  // right edge.
  const viewSwitcher = (
    <div style={{ display: "flex", gap: 4 }}>
      <button
        onClick={() => setViewMode("map")}
        style={{
          fontWeight: viewMode === "map" ? 700 : 400,
          padding: "7px 12px",
          background: "rgba(255,255,255,0.85)",
          backdropFilter: "blur(6px)",
          borderRadius: "var(--radius)",
          boxShadow: "var(--shadow)",
          border: "1px solid var(--border)",
        }}
        title="Show classified features on the map"
      >
        Map
      </button>
      <button
        onClick={() => active.topic && setViewMode("tree")}
        disabled={!active.topic}
        style={{
          fontWeight: viewMode === "tree" ? 700 : 400,
          opacity: active.topic ? 1 : 0.5,
          padding: "7px 12px",
          background: "rgba(255,255,255,0.85)",
          backdropFilter: "blur(6px)",
          borderRadius: "var(--radius)",
          boxShadow: "var(--shadow)",
          border: "1px solid var(--border)",
        }}
        title="Plot this topic's deriver (Producer) trees"
      >
        Producer
      </button>
      <button
        onClick={() => active.topic && setViewMode("categorize")}
        disabled={!active.topic}
        style={{
          fontWeight: viewMode === "categorize" ? 700 : 400,
          opacity: active.topic ? 1 : 0.5,
          padding: "7px 12px",
          background: "rgba(255,255,255,0.85)",
          backdropFilter: "blur(6px)",
          borderRadius: "var(--radius)",
          boxShadow: "var(--shadow)",
          border: "1px solid var(--border)",
        }}
        title="Plot this topic's categorization trees (which category an object gets)"
      >
        Categorize
      </button>
      <button
        onClick={() => active.topic && setViewMode("decision-tree")}
        disabled={!active.topic}
        style={{
          fontWeight: viewMode === "decision-tree" ? 700 : 400,
          opacity: active.topic ? 1 : 0.5,
          padding: "7px 12px",
          background: "rgba(255,255,255,0.85)",
          backdropFilter: "blur(6px)",
          borderRadius: "var(--radius)",
          boxShadow: "var(--shadow)",
          border: "1px solid var(--border)",
        }}
        title="Plot the compiled discrimination net that prunes categorize's first-match walk"
      >
        Decision tree
      </button>
    </div>
  );

  return (
    <div style={{ display: "flex", height: "100%", width: "100%" }}>
      <div style={{ flex: "1 1 auto", minWidth: 0, position: "relative" }}>
        {viewMode === "tree" && active.topic ? (
          <DagView
            topic={active.topic}
            category={active.isTopicConfig ? null : active.name || null}
            mode="deriver"
            extraHeader={viewSwitcher}
          />
        ) : viewMode === "categorize" && active.topic ? (
          <DagView
            topic={active.topic}
            category={active.isTopicConfig ? null : active.name || null}
            mode="category"
            extraHeader={viewSwitcher}
          />
        ) : viewMode === "decision-tree" && active.topic ? (
          <DagView
            topic={active.topic}
            category={active.isTopicConfig ? null : active.name || null}
            mode="decision-tree"
            extraHeader={viewSwitcher}
          />
        ) : (
          <Map
            bounds={bounds}
            data={data}
            cutPoints={cutPoints}
            topicColors={topicColors}
            hiddenTopics={hiddenTopics}
            isolateCategory={isolateCategory}
            focusTarget={focusTarget}
            focusTick={focusTick}
            followSelection={followSelection}
            showNodes={showNodes}
            onBboxSelected={selectBbox}
          />
        )}
        {viewMode === "map" && (
          <div style={{ position: "absolute", top: 12, right: 12, display: "flex", gap: 8 }}>
            <label
              style={{
                padding: "7px 12px",
                background: "rgba(255,255,255,0.85)",
                backdropFilter: "blur(6px)",
                color: "var(--text)",
                fontFamily: "var(--font-ui)",
                fontSize: 13,
                borderRadius: "var(--radius)",
                boxShadow: "var(--shadow)",
                border: "1px solid var(--border)",
                display: "flex",
                alignItems: "center",
                gap: 6,
                cursor: "pointer",
                userSelect: "none",
              }}
            >
              <input type="checkbox" checked={showNodes} onChange={(e) => setShowNodes(e.target.checked)} />
              Show intersections
            </label>
            <label
              style={{
                padding: "7px 12px",
                background: "rgba(255,255,255,0.85)",
                backdropFilter: "blur(6px)",
                color: "var(--text)",
                fontFamily: "var(--font-ui)",
                fontSize: 13,
                borderRadius: "var(--radius)",
                boxShadow: "var(--shadow)",
                border: "1px solid var(--border)",
                display: "flex",
                alignItems: "center",
                gap: 6,
                cursor: "pointer",
                userSelect: "none",
              }}
            >
              <input type="checkbox" checked={followSelection} onChange={(e) => setFollowSelection(e.target.checked)} />
              Follow selection
            </label>
            {viewSwitcher}
          </div>
        )}
        {viewMode === "map" && (!selected || extracting) && (
          <div
            style={{
              position: "absolute",
              top: 12,
              left: 12,
              padding: "7px 12px",
              background: "rgba(255,255,255,0.85)",
              backdropFilter: "blur(6px)",
              color: "var(--text)",
              fontFamily: "var(--font-ui)",
              fontSize: 13,
              borderRadius: "var(--radius)",
              boxShadow: "var(--shadow)",
              border: "1px solid var(--border)",
              pointerEvents: "none",
            }}
          >
            {extracting ? "Extracting…" : "Shift+drag on the map to select an area to edit"}
          </div>
        )}
        {viewMode === "map" && (extractMs !== null || pipelineMs !== null) && (
          <div
            style={{
              position: "absolute",
              bottom: 12,
              left: 12,
              padding: "6px 10px",
              background: "rgba(255,255,255,0.85)",
              backdropFilter: "blur(6px)",
              color: "var(--muted)",
              fontFamily: "var(--font-mono)",
              fontSize: 11,
              borderRadius: "var(--radius)",
              boxShadow: "var(--shadow)",
              border: "1px solid var(--border)",
              pointerEvents: "none",
            }}
          >
            {extractMs !== null && <>extract: {extractMs}ms</>}
            {extractMs !== null && pipelineMs !== null && " · "}
            {pipelineMs !== null && <>pipeline: {pipelineMs}ms</>}
          </div>
        )}
      </div>
      {!collapsed && (
        <Divider axis="x" onMouseDown={(e) => beginResize(e, "x", sidebarWidth, setSidebarWidth, 260, 900, true)} />
      )}
      <div
        style={{
          flex: collapsed ? "0 0 auto" : `0 0 ${sidebarWidth}px`,
          display: "flex",
          flexDirection: "column",
          background: "var(--panel)",
          borderLeft: "1px solid var(--border)",
          boxShadow: "-4px 0 16px rgba(16,24,40,0.04)",
          minWidth: 0,
        }}
      >
        <div
          style={{
            padding: "10px 12px",
            fontFamily: "var(--font-ui)",
            fontSize: 13,
            fontWeight: 600,
            letterSpacing: 0.2,
            borderBottom: "1px solid var(--border)",
            display: "flex",
            alignItems: "center",
            justifyContent: "space-between",
            gap: 8,
          }}
        >
          <span style={{ overflow: "hidden", textOverflow: "ellipsis", whiteSpace: "nowrap" }}>
            {collapsed ? active.topic || "topics" : "topics"}
          </span>
          <button onClick={() => setCollapsed((c) => !c)} title={collapsed ? "Expand" : "Minimize"}>
            {collapsed ? "◀" : "▶"}
          </button>
        </div>
        {!collapsed && configs.length > 0 && (
          <div
            style={{
              padding: "8px 12px",
              fontFamily: "var(--font-ui)",
              fontSize: 13,
              borderBottom: "1px solid var(--border)",
              display: "flex",
              alignItems: "center",
              gap: 8,
            }}
          >
            <span style={{ color: "var(--muted)" }}>config</span>
            <select
              value={currentConfig}
              onChange={(e) => switchConfig(e.target.value)}
              style={{ flex: 1, minWidth: 0 }}
            >
              {configs.map((config) => (
                <option key={config} value={config}>
                  {config}
                </option>
              ))}
            </select>
          </div>
        )}
        {!collapsed && (
          <>
            <div style={{ borderBottom: "1px solid var(--border)", height: topicsListHeight, flexShrink: 0, overflowY: "auto" }}>
              {topics.map((topic) => {
                const expanded = expandedTopics.has(topic);
                const cats = categoriesByTopic[topic] ?? [];
                return (
                  <div key={topic}>
                    <div
                      className="row"
                      onClick={() => toggleTopicExpanded(topic)}
                      style={{
                        padding: "8px 12px",
                        fontFamily: "var(--font-mono)",
                        fontSize: 13,
                        fontWeight: 600,
                        cursor: "pointer",
                        display: "flex",
                        alignItems: "center",
                        gap: 8,
                        opacity: hiddenTopics.has(topic) ? 0.4 : 1,
                        background: "#f7f8fa",
                      }}
                    >
                      <button
                        className="icon-btn"
                        onClick={(e) => {
                          e.stopPropagation();
                          toggleTopicVisibility(topic);
                        }}
                        title={hiddenTopics.has(topic) ? `Show ${topic}` : `Hide ${topic}`}
                        style={{ lineHeight: 1 }}
                      >
                        {hiddenTopics.has(topic) ? "🚫" : "👁"}
                      </button>
                      <span style={{ color: "var(--muted)" }}>{expanded ? "▾" : "▸"}</span>
                      <span
                        style={{
                          display: "inline-block",
                          width: 10,
                          height: 10,
                          borderRadius: "50%",
                          background: topicColors[topic],
                          flexShrink: 0,
                        }}
                      />
                      <span style={{ flex: 1 }}>{topic}</span>
                      <span style={{ color: "var(--muted)" }}>{cats.length}</span>
                    </div>
                    {expanded && (
                      <div style={{ paddingLeft: 16 }}>
                        <div
                          className="row"
                          onClick={() => {
                            setActive({ topic, kind: "", name: "", isTopicConfig: true });
                            if (viewMode === "map") {
                              setManualSelect(true);
                              setFocusTick((t) => t + 1);
                            }
                          }}
                          style={{
                            padding: "7px 12px",
                            fontFamily: "var(--font-mono)",
                            fontSize: 13,
                            fontWeight: 600,
                            cursor: "pointer",
                            display: "flex",
                            alignItems: "center",
                            gap: 6,
                            borderRadius: "var(--radius-sm)",
                            background: active.topic === topic && active.isTopicConfig ? "var(--accent-soft)" : "transparent",
                          }}
                        >
                          topic.json
                        </div>
                        <div style={{ display: "flex", padding: "6px 12px", gap: 6 }}>
                          <input
                            value={newNameByTopic[topic] ?? ""}
                            onChange={(e) => setNewNameByTopic((prev) => ({ ...prev, [topic]: e.target.value }))}
                            onKeyDown={(e) => {
                              if (e.key === "Enter") addCategory(topic);
                            }}
                            placeholder={`new category in ${topic}`}
                            style={{ flex: 1, fontFamily: "var(--font-mono)", fontSize: 13 }}
                          />
                          <button onClick={() => addCategory(topic)} title="Add category">
                            +
                          </button>
                        </div>
                        {cats.map((c) => (
                          <div
                            key={`${c.kind}/${c.name}`}
                            className="row"
                            onClick={() => {
                              if (active.topic === c.topic && active.kind === c.kind && active.name === c.name) {
                                setActive(NO_SELECTION);
                                if (viewMode === "map") setManualSelect(false);
                              } else {
                                setActive(c);
                                if (viewMode === "map") {
                                  setManualSelect(true);
                                  setFocusTick((t) => t + 1);
                                }
                              }
                            }}
                            style={{
                              padding: "7px 12px",
                              fontFamily: "var(--font-mono)",
                              fontSize: 13,
                              cursor: "pointer",
                              display: "flex",
                              alignItems: "center",
                              gap: 6,
                              borderRadius: "var(--radius-sm)",
                              background: c.topic === active.topic && c.kind === active.kind && c.name === active.name ? "var(--accent-soft)" : "transparent",
                            }}
                          >
                            <span style={{ flex: 1, overflow: "hidden", textOverflow: "ellipsis" }}>
                              {c.kind}/{c.name}
                            </span>
                            <button
                              className="icon-btn"
                              onClick={(e) => {
                                e.stopPropagation();
                                deleteCategory(c);
                              }}
                              title={`Delete ${c.kind}/${c.name}`}
                              style={{ lineHeight: 1 }}
                            >
                              ×
                            </button>
                          </div>
                        ))}
                      </div>
                    )}
                  </div>
                );
              })}
            </div>
            {active.topic && (active.name || active.isTopicConfig) && (
              <>
                <Divider axis="y" onMouseDown={(e) => beginResize(e, "y", topicsListHeight, setTopicsListHeight, 80, 640)} />
                <div style={{ display: "flex", flexDirection: "column", flex: 1, minHeight: 0 }}>
                  <div
                    style={{
                      padding: "6px 12px",
                      fontFamily: "var(--font-mono)",
                      fontSize: 11,
                      color: "var(--muted)",
                      background: "#f7f8fa",
                      borderTop: "1px solid var(--border)",
                      borderBottom: "1px solid var(--border)",
                    }}
                  >
                    {active.isTopicConfig ? `${active.topic}/topic.json` : `${active.topic}/${active.kind}/${active.name}.json`}
                  </div>
                  <div style={{ flex: 1, minHeight: 0 }}>
                    <Editor value={text} onChange={setText} />
                  </div>
                </div>
              </>
            )}
            {error && (
              <div
                style={{
                  margin: 10,
                  padding: "8px 10px",
                  background: "var(--danger-bg)",
                  color: "var(--danger-text)",
                  border: "1px solid #f5c2c0",
                  borderRadius: "var(--radius-sm)",
                  fontFamily: "var(--font-mono)",
                  fontSize: 13,
                  whiteSpace: "pre-wrap",
                }}
              >
                {error}
              </div>
            )}
          </>
        )}
      </div>
    </div>
  );
}
