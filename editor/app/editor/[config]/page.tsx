"use client";

import { useEffect, useState } from "react";
import { useParams, useRouter, useSearchParams } from "next/navigation";
import LiveEditor from "@/components/LiveEditor";

// Drives config/topic selection from the URL (`/editor/<config>?topic=<topic>`) before rendering
// the actual editor UI: validates `config`, POSTs `/api/config` to select it server-side, fetches
// its topic list for the dropdown, resolves which topic to show (the `?topic=` param if valid,
// else the first one — normalizing the URL via `router.replace` either way so a refresh/shared
// link always reflects the real selection), then POSTs `/api/topic-select` before handing off to
// `Editor`. Client hooks (`useParams`/`useSearchParams`/`useRouter`), not page props, since this
// page is inherently client-interactive throughout (matches `Editor`'s own all-client-fetch style)
// rather than mixing a server-fetched shell with a client body.
export default function EditorPage() {
  const params = useParams<{ config: string }>();
  const config = decodeURIComponent(params.config);
  const searchParams = useSearchParams();
  const router = useRouter();
  const topicParam = searchParams.get("topic");

  const [topics, setTopics] = useState<string[] | null>(null);
  const [topicReady, setTopicReady] = useState(false);
  const [error, setError] = useState<string | null>(null);

  // 1. Validate + select the config, then list its topics.
  useEffect(() => {
    let ignore = false;
    setTopics(null);
    setTopicReady(false);
    setError(null);
    (async () => {
      const configsRes = await fetch("/api/configs").then((r) => r.json());
      if (ignore) return;
      if (!configsRes.configs?.includes(config)) {
        router.replace("/");
        return;
      }
      const configRes = await fetch("/api/config", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ config }),
      });
      if (ignore) return;
      if (!configRes.ok) {
        const body = await configRes.json().catch(() => ({}));
        setError(body.error || "Failed to select config");
        return;
      }
      const topicsRes = await fetch("/api/topics").then((r) => r.json());
      if (ignore) return;
      setTopics(topicsRes.topics ?? []);
    })();
    return () => {
      ignore = true;
    };
  }, [config, router]);

  // 2. Once topics are known, resolve which one to select (URL param if valid, else the first),
  // normalizing the URL if it doesn't already match, then select it server-side.
  useEffect(() => {
    if (!topics) return;
    if (topics.length === 0) {
      setError(`Config '${config}' has no topics`);
      return;
    }
    const chosen = topicParam && topics.includes(topicParam) ? topicParam : topics[0];
    if (chosen !== topicParam) {
      router.replace(`/editor/${encodeURIComponent(config)}?topic=${encodeURIComponent(chosen)}`);
      return; // effect re-runs once the URL update lands and topicParam matches
    }
    let ignore = false;
    setTopicReady(false);
    fetch("/api/topic-select", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ topic: chosen }),
    }).then((res) => {
      if (ignore) return;
      if (!res.ok) {
        res.json().then((body) => setError(body.error || "Failed to select topic")).catch(() => setError("Failed to select topic"));
        return;
      }
      setTopicReady(true);
    });
    return () => {
      ignore = true;
    };
  }, [config, topics, topicParam, router]);

  function onTopicChange(topic: string) {
    router.replace(`/editor/${encodeURIComponent(config)}?topic=${encodeURIComponent(topic)}`);
  }

  if (error) {
    return (
      <div style={{ display: "flex", alignItems: "center", justifyContent: "center", height: "100%", color: "var(--danger-text)" }}>
        {error}
      </div>
    );
  }
  if (!topics || !topicReady || !topicParam) {
    return (
      <div style={{ display: "flex", alignItems: "center", justifyContent: "center", height: "100%", color: "var(--muted)" }}>
        Loading…
      </div>
    );
  }

  return <LiveEditor config={config} topics={topics} topic={topicParam} onTopicChange={onTopicChange} />;
}
