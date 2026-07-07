import { useEffect, useMemo, useRef, useState } from "react";
import Map from "./Map";
import Editor from "./Editor";

const DEFAULT_KIND = "way";
const DEBOUNCE_MS = 300;
const NEW_CATEGORY_JSON = '{"condition":{}}';

type Category = { topic: string; kind: string; name: string };
const NO_CATEGORY: Category = { topic: "", kind: DEFAULT_KIND, name: "" };

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
  const [extracting, setExtracting] = useState(false);
  const [topics, setTopics] = useState<string[]>([]);
  const [hiddenTopics, setHiddenTopics] = useState<Set<string>>(new Set());
  const [expandedTopics, setExpandedTopics] = useState<Set<string>>(new Set());
  const [categoriesByTopic, setCategoriesByTopic] = useState<Record<string, Category[]>>({});
  const [active, setActive] = useState<Category>(NO_CATEGORY);
  const [newNameByTopic, setNewNameByTopic] = useState<Record<string, string>>({});
  const [topicJson, setTopicJson] = useState<Record<string, string>>({});
  const topicJsonDebounceRef = useRef<Record<string, ReturnType<typeof setTimeout>>>({});
  const [collapsed, setCollapsed] = useState(false);
  const [text, setText] = useState<string>("");
  const [data, setData] = useState<GeoJSON.FeatureCollection | null>(null);
  const [cutPoints, setCutPoints] = useState<GeoJSON.FeatureCollection | null>(null);
  const [error, setError] = useState<string | null>(null);
  const debounceRef = useRef<ReturnType<typeof setTimeout> | null>(null);

  useEffect(() => {
    fetch("/api/bounds")
      .then((r) => r.json())
      .then((d) => {
        setBounds(d.bounds);
        setSelected(d.selected);
      });
    fetch("/api/topics")
      .then((r) => r.json())
      .then((d: { topics: string[] }) => {
        setTopics(d.topics);
        setExpandedTopics((cur) => (cur.size > 0 ? cur : new Set(d.topics[0] ? [d.topics[0]] : [])));
        for (const topic of d.topics) loadCategories(topic);
      });
  }, []);

  function loadCategories(topic: string) {
    fetch(`/api/categories/${encodeURIComponent(topic)}`)
      .then((r) => r.json())
      .then((d: { categories: { kind: string; name: string }[] }) => {
        const cats = d.categories.map((c) => ({ topic, ...c }));
        setCategoriesByTopic((prev) => ({ ...prev, [topic]: cats }));
        // Auto-select the first category loaded, for any topic, as long as nothing is selected yet —
        // `cur.topic === topic` doesn't work here since `cur` starts as NO_CATEGORY (topic: ""),
        // which never equals a real topic name, so nothing was ever auto-selected on load.
        setActive((cur) => (!cur.topic && !cur.name && cats[0] ? cats[0] : cur));
      });
  }

  useEffect(() => {
    if (!active.topic || !active.name) {
      setText(NEW_CATEGORY_JSON);
      return;
    }
    fetch(`/api/category/${encodeURIComponent(active.topic)}/${encodeURIComponent(active.kind)}/${encodeURIComponent(active.name)}`)
      .then((r) => r.json())
      .then((d) => setText(d.json))
      .catch(() => setText(NEW_CATEGORY_JSON));
  }, [active]);

  // Load a topic's topic.json the first time its panel is expanded.
  useEffect(() => {
    for (const topic of expandedTopics) {
      if (topicJson[topic] !== undefined) continue;
      fetch(`/api/topic/${encodeURIComponent(topic)}`)
        .then((r) => r.json())
        .then((d) => setTopicJson((prev) => (prev[topic] !== undefined ? prev : { ...prev, [topic]: d.json })))
        .catch(() => setTopicJson((prev) => (prev[topic] !== undefined ? prev : { ...prev, [topic]: "{}" })));
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [expandedTopics]);

  const categoryColors = useMemo(() => {
    const colors: Record<string, string> = {};
    for (const cats of Object.values(categoriesByTopic)) {
      for (const c of cats) colors[`${c.topic}/${c.name}`] = hashColor(`${c.topic}/${c.name}`);
    }
    return colors;
  }, [categoriesByTopic]);

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
        setData(null);
        if (text) classify(text);
      } else {
        setError(body.error || "Unknown error");
      }
    } finally {
      setExtracting(false);
    }
  }

  useEffect(() => {
    if (!text || !active.topic || !active.name) return;
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
    } else if (selected) {
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
      return next;
    });
  }

  async function classify(json: string, category: Category = active) {
    try {
      JSON.parse(json);
    } catch (err) {
      setError(`Invalid JSON: ${(err as Error).message}`);
      return;
    }
    const res = await fetch("/api/classify", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ topic: category.topic, kind: category.kind, name: category.name, json }),
    });
    const body = await res.json();
    if (res.ok) {
      setError(null);
      setData(body);
      setCutPoints(body.cutPoints ?? null);
    } else {
      setError(body.error || "Unknown error");
    }
  }

  async function classifyTopic(topic: string, json: string) {
    try {
      JSON.parse(json);
    } catch (err) {
      setError(`Invalid JSON: ${(err as Error).message}`);
      return;
    }
    const res = await fetch(`/api/topic/${encodeURIComponent(topic)}`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ json }),
    });
    const body = await res.json();
    if (res.ok) {
      setError(null);
      setData(body);
      setCutPoints(body.cutPoints ?? null);
    } else {
      setError(body.error || "Unknown error");
    }
  }

  function onTopicJsonChange(topic: string, value: string) {
    setTopicJson((prev) => ({ ...prev, [topic]: value }));
    const timers = topicJsonDebounceRef.current;
    if (timers[topic]) clearTimeout(timers[topic]);
    timers[topic] = setTimeout(() => classifyTopic(topic, value), DEBOUNCE_MS);
  }

  return (
    <div style={{ display: "flex", height: "100%", width: "100%" }}>
      <div style={{ flex: "1 1 60%", position: "relative" }}>
        <Map bounds={bounds} data={data} cutPoints={cutPoints} categoryColors={categoryColors} hiddenTopics={hiddenTopics} onBboxSelected={selectBbox} />
        {(!selected || extracting) && (
          <div
            style={{
              position: "absolute",
              top: 10,
              left: 10,
              padding: "6px 10px",
              background: "rgba(0,0,0,0.7)",
              color: "#fff",
              fontFamily: "sans-serif",
              fontSize: 13,
              borderRadius: 4,
              pointerEvents: "none",
            }}
          >
            {extracting ? "Extracting…" : "Shift+drag on the map to select an area to edit"}
          </div>
        )}
      </div>
      <div
        style={{
          flex: collapsed ? "0 0 auto" : "1 1 40%",
          display: "flex",
          flexDirection: "column",
          borderLeft: "1px solid #ccc",
          minWidth: 0,
        }}
      >
        <div
          style={{
            padding: "6px 10px",
            fontFamily: "sans-serif",
            fontSize: 13,
            borderBottom: "1px solid #ccc",
            display: "flex",
            alignItems: "center",
            justifyContent: "space-between",
            gap: 8,
          }}
        >
          <span style={{ overflow: "hidden", textOverflow: "ellipsis", whiteSpace: "nowrap" }}>
            {active.topic && active.name
              ? collapsed
                ? active.topic
                : `${active.topic}/${active.kind}/${active.name}.json`
              : "no category selected"}
          </span>
          <button onClick={() => setCollapsed((c) => !c)} title={collapsed ? "Expand" : "Minimize"}>
            {collapsed ? "◀" : "▶"}
          </button>
        </div>
        {!collapsed && (
          <>
            <div style={{ borderBottom: "1px solid #ccc", maxHeight: 320, overflowY: "auto" }}>
              {topics.map((topic) => {
                const expanded = expandedTopics.has(topic);
                const cats = categoriesByTopic[topic] ?? [];
                return (
                  <div key={topic}>
                    <div
                      onClick={() => toggleTopicExpanded(topic)}
                      style={{
                        padding: "5px 10px",
                        fontFamily: "monospace",
                        fontSize: 12,
                        fontWeight: "bold",
                        cursor: "pointer",
                        display: "flex",
                        alignItems: "center",
                        gap: 6,
                        opacity: hiddenTopics.has(topic) ? 0.4 : 1,
                        background: "#0002",
                      }}
                    >
                      <button
                        onClick={(e) => {
                          e.stopPropagation();
                          toggleTopicVisibility(topic);
                        }}
                        title={hiddenTopics.has(topic) ? `Show ${topic}` : `Hide ${topic}`}
                        style={{ lineHeight: 1 }}
                      >
                        {hiddenTopics.has(topic) ? "🚫" : "👁"}
                      </button>
                      <span>{expanded ? "▾" : "▸"}</span>
                      <span style={{ flex: 1 }}>{topic}</span>
                      <span style={{ color: "#888" }}>{cats.length}</span>
                    </div>
                    {expanded && (
                      <div style={{ paddingLeft: 14 }}>
                        {cats.map((c) => (
                          <div
                            key={`${c.kind}/${c.name}`}
                            onClick={() => setActive(c)}
                            style={{
                              padding: "5px 10px",
                              fontFamily: "monospace",
                              fontSize: 12,
                              cursor: "pointer",
                              display: "flex",
                              alignItems: "center",
                              gap: 6,
                              background: c.topic === active.topic && c.kind === active.kind && c.name === active.name ? "#2a7be633" : "transparent",
                            }}
                          >
                            <span
                              style={{
                                display: "inline-block",
                                width: 10,
                                height: 10,
                                borderRadius: "50%",
                                background: categoryColors[`${c.topic}/${c.name}`],
                                flexShrink: 0,
                              }}
                            />
                            <span style={{ flex: 1, overflow: "hidden", textOverflow: "ellipsis" }}>
                              {c.kind}/{c.name}
                            </span>
                            <button
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
                        <div style={{ display: "flex", padding: "5px 10px", gap: 4 }}>
                          <input
                            value={newNameByTopic[topic] ?? ""}
                            onChange={(e) => setNewNameByTopic((prev) => ({ ...prev, [topic]: e.target.value }))}
                            onKeyDown={(e) => {
                              if (e.key === "Enter") addCategory(topic);
                            }}
                            placeholder={`new category in ${topic}`}
                            style={{ flex: 1, fontFamily: "monospace", fontSize: 12 }}
                          />
                          <button onClick={() => addCategory(topic)} title="Add category">
                            +
                          </button>
                        </div>
                        <div style={{ padding: "5px 10px 8px" }}>
                          <div style={{ fontFamily: "monospace", fontSize: 11, color: "#888", marginBottom: 3 }}>
                            {topic}/topic.json
                          </div>
                          <div style={{ height: 160, border: "1px solid #ccc" }}>
                            <Editor value={topicJson[topic] ?? ""} onChange={(v) => onTopicJsonChange(topic, v)} />
                          </div>
                        </div>
                      </div>
                    )}
                  </div>
                );
              })}
            </div>
            <div style={{ flex: 1, minHeight: 0 }}>
              <Editor value={text} onChange={setText} />
            </div>
            {error && (
              <div style={{ padding: "8px 10px", background: "#3a1f1f", color: "#ffb4b4", fontFamily: "monospace", fontSize: 12, whiteSpace: "pre-wrap" }}>
                {error}
              </div>
            )}
          </>
        )}
      </div>
    </div>
  );
}
