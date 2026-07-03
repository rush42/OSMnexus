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

- **Decision tree / discrimination net for `categorize` — BUILT, MEASURED, REVERTED. Revisit only
  if the scale changes (see trigger).** A field-branching tree (branch on `highway` etc. to a small
  candidate leaf, then first-match the residual conditions) was fully implemented behind a
  `--classifier tree|linear` flag, with a `--dump-category-tree` JSON dump and a differential test
  vs linear. Recoverable at **commit `54c2c95b4`** (impl + flag + dump; the core is `f4825a817` /
  `ef7b82246`); removed in `76675e7b8`. Sound pruning via three-valued (Kleene) eval of each node's
  NNF condition — no DNF blowup — so guard predicates (`contains`/numeric/`sanitize`/parent-tag)
  stay at leaves and correctness never depends on the tree being clever.
  - **Why reverted:** ~5% wall on Brandenburg (and only on roads — 27 leaves, ~2.4 candidates/leaf;
    bikelanes degenerates to 141 leaves, ~11.5/leaf, because its distinctions are sanitized/
    non-equality predicates that can't branch). Too little runtime win + too much clever code, and
    the tree-as-docs read poorly for the same reason (huge guard-leaves).
  - **Trigger to revisit:** the win scales with **category count** and **absence of the
    `element_filter`**. If the topic/category set balloons (toward the full Lua set) or the explicit
    filter is dropped (the tree subsumes it — unmatched ways fall to no-category leaves), linear
    becomes O(categories × ways) on an unfiltered firehose and the tree becomes the obvious
    structure, not over-engineering. ~5% is close to its *worst* case (pre-filtered, few categories).
  - **Bonus if revived:** the same tree makes the disjointness check O(leaves) instead of the current
    O(n²) pairwise-DNF (`overlap.rs` / `categories_are_disjoint`).
  - **Lesson kept even though the code isn't:** the `element_filter` is emergent from the categories,
    and bikelanes classification is *not* tag-value-shaped (so no tree/taxonomy diagram reads cleanly
    for it) — to make bikelanes prune/plot well, expose an un-sanitized discriminator, not machinery.

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
