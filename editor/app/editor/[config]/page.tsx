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
  // The topic `/api/topic-select` last succeeded for — compared against `topicParam` below rather
  // than a plain ready boolean. A boolean would go stale across a topic change: React re-renders
  // this page with the new `topicParam` (from the `router.replace` in effect 2) while the old
  // `true` is still sitting in state, since resetting it only happens inside that same effect,
  // which — for child components — commits *after* `LiveEditor`'s own effects (React runs child
  // effects before parent effects). `LiveEditor` fires its topic-keyed fetches (`loadCategories`)
  // the instant it sees the new `topic` prop, so with a boolean it would briefly believe the new
  // topic was ready and query the server before `/api/topic-select` for it had even been sent —
  // the server still has the *previous* topic selected at that point, so every such request 400s.
  // Comparing against the specific topic instead closes that window: it only reads as ready once
  // this effect's own POST for *that* topic has actually resolved.
  const [readyTopic, setReadyTopic] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  // 1. Validate + select the config, then list its topics.
  useEffect(() => {
    let ignore = false;
    setTopics(null);
    setReadyTopic(null);
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
      setReadyTopic(chosen);
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
  // Only the very first selection ever (for this config) blocks on `readyTopic` here — once
  // `LiveEditor` has mounted at least once, later topic switches keep it mounted and pass
  // `readyTopic === topicParam` through as its own `topicReady` prop instead, so it can wait out a
  // switch internally (map staying on the outgoing topic meanwhile) rather than this page unmounting
  // it back to a blank "Loading…" screen and losing all that state on every switch.
  if (!topics || !topicParam || readyTopic === null) {
    return (
      <div style={{ display: "flex", alignItems: "center", justifyContent: "center", height: "100%", color: "var(--muted)" }}>
        Loading…
      </div>
    );
  }

  return (
    <LiveEditor
      config={config}
      topics={topics}
      topic={topicParam}
      topicReady={readyTopic === topicParam}
      onTopicChange={onTopicChange}
    />
  );
}
