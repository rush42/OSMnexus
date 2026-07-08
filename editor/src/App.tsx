import { useEffect, useMemo, useRef, useState } from "react";
import Map from "./Map";
import Editor from "./Editor";

const DEFAULT_KIND = "way";
const DEBOUNCE_MS = 300;
const NEW_CATEGORY_JSON = '{"condition":{}}';
const MIN_BBOX_M = 100;
const DEFAULT_MAX_BBOX_M = 3000; // Used until /api/bounds' server-configured value (MAX_BBOX_M env var) loads.
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
  const [newNameByTopic, setNewNameByTopic] = useState<Record<string, string>>({});
  const [collapsed, setCollapsed] = useState(false);
  const [showNodes, setShowNodes] = useState(false);
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
  // shared by the initial mount and by switchConfig (after the server-side selection changes).
  function loadTopics() {
    fetch("/api/topics")
      .then((r) => r.json())
      .then((d: { topics: string[] }) => {
        setTopics(d.topics);
        setExpandedTopics((cur) => (cur.size > 0 ? cur : new Set(d.topics[0] ? [d.topics[0]] : [])));
        for (const topic of d.topics) loadCategories(topic);
      });
  }

  function loadCategories(topic: string) {
    fetch(`/api/categories/${encodeURIComponent(topic)}`)
      .then((r) => r.json())
      .then((d: { categories: { kind: string; name: string }[] }) => {
        const cats = d.categories.map((c) => ({ topic, ...c }));
        setCategoriesByTopic((prev) => ({ ...prev, [topic]: cats }));
        // Auto-select the first category loaded, for any topic, as long as nothing is selected yet —
        // `cur.topic === topic` doesn't work here since `cur` starts as NO_SELECTION (topic: ""),
        // which never equals a real topic name, so nothing was ever auto-selected on load.
        setActive((cur) => (!cur.topic && !cur.name && cats[0] ? cats[0] : cur));
      });
  }

  // Switches the server's selected config directory, then resets every topic-scoped piece of
  // state (categories/selection differ entirely between configs) and reloads — same load path as
  // the initial mount, so the freshly switched config auto-selects its first category and (if a
  // bbox is already chosen) auto-classifies via the existing [active]/[text] effects.
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
    setData(null);
    setCutPoints(null);
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

  return (
    <div style={{ display: "flex", height: "100%", width: "100%" }}>
      <div style={{ flex: "1 1 60%", position: "relative" }}>
        <Map
          bounds={bounds}
          data={data}
          cutPoints={cutPoints}
          topicColors={topicColors}
          hiddenTopics={hiddenTopics}
          isolateCategory={active.name && !active.isTopicConfig ? { topic: active.topic, name: active.name } : null}
          showNodes={showNodes}
          onBboxSelected={selectBbox}
        />
        <label
          style={{
            position: "absolute",
            top: 12,
            right: 12,
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
        {(!selected || extracting) && (
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
        {(extractMs !== null || pipelineMs !== null) && (
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
      <div
        style={{
          flex: collapsed ? "0 0 auto" : "1 1 40%",
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
              fontSize: 12,
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
            <div style={{ borderBottom: "1px solid var(--border)", maxHeight: 320, overflowY: "auto" }}>
              {topics.map((topic) => {
                const expanded = expandedTopics.has(topic);
                const cats = categoriesByTopic[topic] ?? [];
                return (
                  <div key={topic}>
                    <div
                      className="row"
                      onClick={() => toggleTopicExpanded(topic)}
                      style={{
                        padding: "6px 12px",
                        fontFamily: "var(--font-mono)",
                        fontSize: 12,
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
                          onClick={() => setActive({ topic, kind: "", name: "", isTopicConfig: true })}
                          style={{
                            padding: "6px 12px",
                            fontFamily: "var(--font-mono)",
                            fontSize: 12,
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
                            style={{ flex: 1, fontFamily: "var(--font-mono)", fontSize: 12 }}
                          />
                          <button onClick={() => addCategory(topic)} title="Add category">
                            +
                          </button>
                        </div>
                        {cats.map((c) => (
                          <div
                            key={`${c.kind}/${c.name}`}
                            className="row"
                            onClick={() => setActive(c)}
                            style={{
                              padding: "6px 12px",
                              fontFamily: "var(--font-mono)",
                              fontSize: 12,
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
                    overflow: "hidden",
                    textOverflow: "ellipsis",
                    whiteSpace: "nowrap",
                  }}
                >
                  {active.isTopicConfig ? `${active.topic}/topic.json` : `${active.topic}/${active.kind}/${active.name}.json`}
                </div>
                <div style={{ flex: 1, minHeight: 0 }}>
                  <Editor value={text} onChange={setText} />
                </div>
              </div>
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
                  fontSize: 12,
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
