import { useEffect, useRef, useState } from "react";
import Map from "./Map";
import Editor from "./Editor";

const TOPIC = "bikelanes_simple";
const KIND = "way";
const NAME = "bikeway";
const DEBOUNCE_MS = 300;

export default function App() {
  const [bounds, setBounds] = useState<[number, number, number, number] | null>(null);
  const [text, setText] = useState<string>("");
  const [data, setData] = useState<GeoJSON.FeatureCollection | null>(null);
  const [error, setError] = useState<string | null>(null);
  const debounceRef = useRef<ReturnType<typeof setTimeout> | null>(null);

  useEffect(() => {
    fetch("/api/bounds")
      .then((r) => r.json())
      .then((d) => setBounds(d.bounds));
    fetch(`/api/category/${TOPIC}/${KIND}/${NAME}`)
      .then((r) => r.json())
      .then((d) => setText(d.json));
  }, []);

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
      <div style={{ flex: "1 1 60%" }}>
        <Map bounds={bounds} data={data} />
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
