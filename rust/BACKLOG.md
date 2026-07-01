# Backlog

Deferred ideas / nice-to-haves for the Rust pipeline. Not blocking anything.

- **Drop Pass C via tag-only categorization + materialized ways.** Today the reader decodes the
  way region twice: Pass A (`filter ways` → per-node use counts / needed-node set) and Pass C
  (`build geometries` → resolve geometry + categorize + emit). The idea: run **categorization in
  Pass A** (tag-only) and keep each surviving way's full `WayData` (tags + node_refs + meta +
  category) in memory, so Pass C's re-decode disappears — Pass B builds coords, then an in-memory
  parallel map resolves geometry and runs derivers/minzoom/length afterwards.
  - Requires retaining tags too, not just node_refs (tags are consumed by categorization/extraction
    and dominate the retained memory). Est. Germany peak ~9 GB (~7 GB coords + ~1.5–2 GB ways).
  - **Blocker: categorization is not tag-only.** The `crossing` category gates on `length ≤ 100`,
    which needs geometry. Verified on Germany: removing that gate reclassifies **80 ways**
    (needsClarification → crossing), so it's a real dependency, not data-inert. So a clean version
    needs a **two-stage categorize**: tag-only categories in Pass A, length-gated ones refined
    post-geometry — or an accepted ~80-way divergence from the Lua source.
  - **Value is structural, not speed.** The pipeline is not decode-bound (8 threads → only ~1.5×;
    `--threads 4` ≈ `--threads 8`), so removing one of three decode passes likely saves little wall
    time. The real payoff is ways-as-in-memory-objects, a better base for the graph builder.
  - Scope to the fast path only (fallback stays streaming), gated behind a flag.
