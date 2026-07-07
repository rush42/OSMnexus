import { useEffect, useRef, useState } from "react";
import Map from "./Map";
import Editor from "./Editor";

const TOPIC = "example";
const DEFAULT_KIND = "way";
const DEBOUNCE_MS = 300;
const NEW_CATEGORY_JSON = '{"condition":{}}';

type Category = { kind: string; name: string };

export default function App() {
  const [bounds, setBounds] = useState<[number, number, number, number] | null>(null);
  const [selected, setSelected] = useState(false);
  const [extracting, setExtracting] = useState(false);
  const [categories, setCategories] = useState<Category[]>([]);
  const [active, setActive] = useState<Category>({ kind: DEFAULT_KIND, name: "example" });
  const [newName, setNewName] = useState("");
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
    loadCategories();
  }, []);

  useEffect(() => {
    fetch(`/api/category/${TOPIC}/${active.kind}/${active.name}`)
      .then((r) => r.json())
      .then((d) => setText(d.json))
      .catch(() => setText(NEW_CATEGORY_JSON));
  }, [active]);

  function loadCategories() {
    fetch(`/api/categories/${TOPIC}`)
      .then((r) => r.json())
      .then((d) => setCategories(d.categories));
  }

  async function addCategory() {
    const name = newName.trim();
    if (!name) return;
    setNewName("");
    setActive({ kind: DEFAULT_KIND, name });
    setText(NEW_CATEGORY_JSON);
    await classify(NEW_CATEGORY_JSON, { kind: DEFAULT_KIND, name });
    loadCategories();
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
    if (!text) return;
    if (debounceRef.current) clearTimeout(debounceRef.current);
    debounceRef.current = setTimeout(() => {
      classify(text);
    }, DEBOUNCE_MS);
    return () => {
      if (debounceRef.current) clearTimeout(debounceRef.current);
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [text, active]);

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
      body: JSON.stringify({ topic: TOPIC, kind: category.kind, name: category.name, json }),
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

  return (
    <div style={{ display: "flex", height: "100%", width: "100%" }}>
      <div style={{ flex: "1 1 60%", position: "relative" }}>
        <Map bounds={bounds} data={data} cutPoints={cutPoints} onBboxSelected={selectBbox} />
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
            {collapsed ? TOPIC : `${TOPIC}/${active.kind}/${active.name}.json`}
          </span>
          <button onClick={() => setCollapsed((c) => !c)} title={collapsed ? "Expand" : "Minimize"}>
            {collapsed ? "◀" : "▶"}
          </button>
        </div>
        {!collapsed && (
          <>
            <div style={{ borderBottom: "1px solid #ccc", maxHeight: 160, overflowY: "auto" }}>
              {categories.map((c) => (
                <div
                  key={`${c.kind}/${c.name}`}
                  onClick={() => setActive(c)}
                  style={{
                    padding: "5px 10px",
                    fontFamily: "monospace",
                    fontSize: 12,
                    cursor: "pointer",
                    background: c.kind === active.kind && c.name === active.name ? "#2a7be633" : "transparent",
                  }}
                >
                  {c.kind}/{c.name}
                </div>
              ))}
              <div style={{ display: "flex", padding: "5px 10px", gap: 4 }}>
                <input
                  value={newName}
                  onChange={(e) => setNewName(e.target.value)}
                  onKeyDown={(e) => {
                    if (e.key === "Enter") addCategory();
                  }}
                  placeholder="new category name"
                  style={{ flex: 1, fontFamily: "monospace", fontSize: 12 }}
                />
                <button onClick={addCategory} title="Add category">
                  +
                </button>
              </div>
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
