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

- **Compile categories into one decision tree (categorize + disjointness in one structure).**
  `categorize` is already a compiled priority list (first-match, no runtime excludes — done). The
  next step is a real **decision tree / discrimination net**: branch on discriminating tags
  (`highway`, …) to reach a leaf holding the co-matching category set, so classification is
  ~O(depth) instead of first-match over the whole list. The *same* tree makes the disjointness
  check **non-quadratic**: at each leaf, verify the co-matching set is totally exclude-ordered
  (O(leaves) vs the current O(n²) pairwise-DNF).
  - Build on the `overlap.rs` DNF machinery; the disjointness invariant is the precondition.
  - **Hard part:** non-equality predicates (`contains`, `starts_with`, numeric, `sanitize`,
    parent-tag, macros) don't make clean branches — they become **guard nodes** evaluated at a
    leaf, so it's a decision-tree-with-guards, not a pure trie. First-match/excludes semantics must
    be preserved; needs a differential test vs current `categorize`.
  - Justify by the **runtime** win (categorize is the ~40% Pass-C bucket; already −40% from FxHash +
    ordered list, but a tree could push further); the faster disjointness check is a free bonus.

- **Speed up / gate the `categories_are_disjoint` test.** It's ~57 s idle but the DNF conversion is
  exponential in condition size (a few big-condition categories dominate; the 78 non-excluding
  pairs and exclude-skip are already cheap). Cheapest: mark `#[ignore]` and run it in CI only.
  Better: replace full-DNF-cross-product with a short-circuiting satisfiability check (stop at the
  first consistent term — we only need *does an overlap exist*). The decision-tree item above
  subsumes this. (Lazy-DNF-build was tried and reverted: only 4/26 categories are skippable, no
  measurable gain.)

- **Trim the Pass-C "other" bucket (~33%: transforms + side-split).** After FxHash + move-not-clone,
  the profiler (`PASS_C_PROFILE=1`) shows "other" is the largest classify sub-bucket — dominated by
  the `lifecycle` transform and side-split unnesting *logic*, not the raw clones. Guard them on tag
  presence: skip `lifecycle` when no `construction:*`/`proposed:*`/… keys; skip the side-split scan
  when no `cycleway:*` keys. Verify byte-identical.

- **DB write throughput (only if it becomes the wall).** Currently the single-consumer COPY keeps
  pace with the producer (co-terminal), so it is *not* the bottleneck now. If the producer is sped
  up enough to flip that, options: parallel COPY connections per topic, or binary `COPY` (FORMAT
  BINARY) — moderate work (hand-encode each column in the pgwire binary layout). Measure first.
