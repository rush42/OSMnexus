import Link from "next/link";
import { listConfigs } from "@/lib/liveEditor";

// Reads the live `configs/` dir on every request (not a build-time constant) — a config can be
// added/removed on disk between requests, and this is the one page whose whole job is showing
// that list accurately.
export const dynamic = "force-dynamic";

// Server component: no client interactivity needed here (just a list of links), so this fetches
// `listConfigs()` directly instead of round-tripping through `/api/configs` — unlike the editor
// page (`app/editor/[config]/page.tsx`), which is inherently client-interactive (map/dropdown/live
// editing) and stays consistent with the rest of the app's client-fetch pattern.
export default async function StartPage() {
  const configs = await listConfigs();
  return (
    <main
      style={{
        display: "flex",
        flexDirection: "column",
        alignItems: "center",
        justifyContent: "center",
        height: "100%",
        gap: 24,
      }}
    >
      <h1 style={{ fontSize: 20, fontWeight: 600 }}>OSMnexus Live Editor</h1>
      <p style={{ color: "var(--muted)", margin: 0 }}>Select a config to start editing.</p>
      <ul
        style={{
          listStyle: "none",
          margin: 0,
          padding: 0,
          display: "flex",
          flexDirection: "column",
          gap: 8,
          minWidth: 280,
        }}
      >
        {configs.map((config) => (
          <li key={config}>
            <Link
              href={`/editor/${encodeURIComponent(config)}`}
              style={{
                display: "block",
                padding: "10px 16px",
                background: "var(--panel)",
                border: "1px solid var(--border)",
                borderRadius: "var(--radius)",
                boxShadow: "var(--shadow-sm)",
                fontFamily: "var(--font-mono)",
                fontSize: 14,
                textDecoration: "none",
                color: "var(--text)",
              }}
            >
              {config}
            </Link>
          </li>
        ))}
      </ul>
    </main>
  );
}
