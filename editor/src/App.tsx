import { useEffect, useRef, useState } from "react";
import Map from "./Map";
import Editor from "./Editor";

const TOPIC = "bikelanes_simple";
const KIND = "way";
const NAME = "bikeway";
const DEBOUNCE_MS = 300;

export default function App() {
  const [bounds, setBounds] = useState<[number, number, number, number] | null>(null);
  const [selected, setSelected] = useState(false);
  const [extracting, setExtracting] = useState(false);
  const [text, setText] = useState<string>("");
  const [data, setData] = useState<GeoJSON.FeatureCollection | null>(null);
  const [error, setError] = useState<string | null>(null);
  const debounceRef = useRef<ReturnType<typeof setTimeout> | null>(null);

  useEffect(() => {
    fetch("/api/bounds")
      .then((r) => r.json())
      .then((d) => {
        setBounds(d.bounds);
        setSelected(d.selected);
      });
    fetch(`/api/category/${TOPIC}/${KIND}/${NAME}`)
      .then((r) => r.json())
      .then((d) => setText(d.json));
  }, []);

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
  }, [text]);

  async function classify(json: string) {
    try {
      JSON.parse(json);
    } catch (err) {
      setError(`Invalid JSON: ${(err as Error).message}`);
      return;
    }
    const res = await fetch("/api/classify", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ topic: TOPIC, kind: KIND, name: NAME, json }),
    });
    const body = await res.json();
    if (res.ok) {
      setError(null);
      setData(body);
    } else {
      setError(body.error || "Unknown error");
    }
  }

  return (
    <div style={{ display: "flex", height: "100%", width: "100%" }}>
      <div style={{ flex: "1 1 60%", position: "relative" }}>
        <Map bounds={bounds} data={data} onBboxSelected={selectBbox} />
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
      <div style={{ flex: "1 1 40%", display: "flex", flexDirection: "column", borderLeft: "1px solid #ccc" }}>
        <div style={{ padding: "6px 10px", fontFamily: "sans-serif", fontSize: 13, borderBottom: "1px solid #ccc" }}>
          {TOPIC}/{KIND}/{NAME}.json
        </div>
        <div style={{ flex: 1, minHeight: 0 }}>
          <Editor value={text} onChange={setText} />
        </div>
        {error && (
          <div style={{ padding: "8px 10px", background: "#3a1f1f", color: "#ffb4b4", fontFamily: "monospace", fontSize: 12, whiteSpace: "pre-wrap" }}>
            {error}
          </div>
        )}
      </div>
    </div>
  );
}
