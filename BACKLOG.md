# Backlog

Deferred ideas / nice-to-haves for the Rust pipeline. Not blocking anything.

- **Flat arenas + a sort-merge node locator, to replace the `id → coord` hashmap — DESIGNED, NOT
  BUILT. Baseline is now 12211 MB** (germany-latest, `configs/tilda`, 4 threads, CSV, peak RSS),
  down from 15366 MB after the three quick wins in the Unreleased changelog entry. Do this only if
  that number is still unacceptable; it is a substantially larger change than what it replaces.
  - **Shape.** Pass A writes way node refs into a flat `ref_ids: Vec<i64>` with a per-way
    `offsets: Vec<u64>`, replacing `way_refs: FxHashMap<i64, (Vec<i64>, u32)>` and its per-way
    allocation. A join index `Vec<(i64 node_id, u64 slot)>`, rayon-`par_sort_unstable`-ed by node
    id, replaces the coord hashmap. Pass B allocates `coords` parallel to `ref_ids` (NaN sentinel =
    missing; **not** all-zeros, since (0,0) is a valid coordinate) and merge-walks it: each node
    blob covers a contiguous id range, so it `partition_point`s to its start and walks forward.
    Materialize then reads each way's coords contiguously *in way order* straight from the arena.
  - **What it removes:** the coord hashmap, `use_counts` entirely (runs of equal node id in the
    sorted join index give the `shared` flag directly), the random hashmap probe per node ref in
    `resolve_geometry`, and the f64-widened `resolved: FxHashMap<i64, OsmWay>` in `materialize.rs`.
    Also one hash probe per node in the file — currently ~400 M probes into a ~100 M-entry map.
    Rough budget: ~32 B/ref against today's ~75 B/ref equivalent.
  - **Hazards found while designing it (each has bitten a similar rewrite before):**
    - `shared` must be derived only from `mask != 0` slots. Relation-member-only ways don't
      contribute to `use_counts` today, and deriving it from raw runs would silently over-cut ways.
      Carry a per-slot `KEPT` flag byte.
    - `use_counts` counts *occurrences*, not distinct ways — a closed way gives its own closing node
      a count of 2. Run-length counting over slots reproduces that exactly; deduplicating by way id
      would silently change the graph. Worth a roundabout regression test.
    - Node blobs are **not** provably id-disjoint (`blob_index.rs` verifies element type per blob,
      not id monotonicity across them), and a block can mix dense and sparse groups. So the
      "disjoint slices, `split_at_mut`" version is unsound. Use `Vec<AtomicU64>` with `Relaxed`
      stores — a plain `mov` on x86-64, zero measurable cost, correct unconditionally.
    - `endpoints` must stay defined on **raw** `refs.first()/last()`, not the NaN-filtered
      resolvable ones, or the graph-vertex count changes.
    - Use `u64` slot indices, not `u32`: `(i64, u32)` pads to 16 B anyway so it's free, and
      `europe-latest.osm.pbf` (in this repo) has enough way-nodes to overflow a u32 arena index.
  - **Don't bother interleaving materialization into the node pass.** The original motivation for
    this work was "free a way's coordinates once it's emitted", but way rows already stream out one
    at a time (`790c02a02`), so nothing accumulates to free; and a flat arena can't return a
    completed way's slice to the allocator anyway. Giving each way its own allocation so it *could*
    be freed costs ~30 B/way of header + pointer to save ~8 B/ref, which at OSM's ~10 refs/way is a
    wash before allocator churn. The payoff would be latency, not RAM. If a further RAM cut is
    genuinely needed, mmap-spill the join index instead (it's written once sequentially and read
    once in sorted order — a perfect page-cache candidate) for roughly a tenth of the complexity.
  - **Verification note:** byte-identical CSV is not achievable and never was — row order varies
    run to run. Compare sorted. If a config ever enables `"geometry": {"way": ["graph"]}`, internal
    vertex ids also need canonicalizing (join `edges` to `nodes` and substitute `osm_id`).

- **No shipped config enables `"geometry": {"way": ["graph"]}`.** `grep` finds it nowhere in
  `configs/`, and `git log -S` finds it nowhere in the history either — so `plan.any_way_graph` is
  false on every config in the repo, and `assign_node_ids`, `MaterializedGeometry::node_rows`, and
  `node_ids` are all dead weight at runtime today. Worth knowing before optimizing any of them: on
  paper `node_rows` looks like a ~2 GB buffer held across the whole materialize phase (~88 B per
  graph vertex, drained only at the end of `main.rs`), and it is — but only for a config that
  doesn't currently exist. Fix it when the graph path is actually used, not before.

- **Decision tree / discrimination net for `categorize` — BUILT, MEASURED, LIVE (this note
  previously said "reverted"; it wasn't — `cats.tree` is wired into `categorize` today via
  `decision_tree::build` in `CategoriesFile::build_order`, `categories.rs`). Correcting the record
  below; the historical measurement still stands.** A field-branching tree (branch on `highway` etc.
  to a small candidate leaf, then first-match the residual conditions), with `categorize_linear` kept
  as the unpruned reference for a differential test. Original impl at commit `54c2c95b4` (core
  `f4825a817` / `ef7b82246`). Sound pruning via three-valued (Kleene) eval of each node's NNF
  condition — no DNF blowup — so guard predicates (`contains`/numeric/`sanitize`/parent-tag) stay at
  leaves and correctness never depends on the tree being clever.
  - **Measured win:** ~5% wall on Brandenburg (and only on roads — 27 leaves, ~2.4 candidates/leaf;
    bikelanes degenerates to 141 leaves, ~11.5/leaf, because its distinctions are sanitized/
    non-equality predicates that can't branch). Small, but kept since it's cheap to keep.
  - **Re-measured at scale (2026-07-09, `germany-latest.osm.pbf`, `--threads 1`, tilda config,
    tree vs `categorize_linear` swapped in via a throwaway build):** tree Pass A 834.6s vs linear
    1044.4s (**~20% faster**), total read+process 1138.3s vs 1347.7s (**~18% faster**). Output
    row counts identical across all four tables (bikelanes/roads/edges/nodes) — no correctness
    difference, consistent with the `tree_matches_linear` differential test. Confirms the trigger
    below: Germany has ~16x Brandenburg's data and far more `highway` value diversity, and the win
    roughly quadrupled. **Decision: keep the tree** — already live, and the win only grows with
    scale.
  - **Trigger to revisit further investment:** the win scales with **category count** and **absence
    of the `element_filter`**. If the topic/category set balloons (toward the full Lua set) or the
    explicit filter is dropped (the tree subsumes it — unmatched ways fall to no-category leaves),
    linear becomes O(categories × ways) on an unfiltered firehose and the tree becomes the obvious
    structure, not over-engineering. ~5% was close to its *worst* case (pre-filtered, few
    categories, small extract) — the Germany measurement above is the more representative number.
  - **Bonus if revived:** the same tree makes the disjointness check O(leaves) instead of the current
    O(n²) pairwise-DNF (`overlap.rs` / `categories_are_disjoint`).
  - **Lesson:** the `element_filter` is emergent from the categories, and bikelanes classification is
    *not* tag-value-shaped (so no tree/taxonomy diagram reads cleanly for it) — to make bikelanes
    prune/plot well, expose an un-sanitized discriminator, not machinery.

- **`RuleIndex` (hash-dispatch for `classify_rules`) — BUILT, MEASURED, NOT MERGED (parked on branch
  `classify-rule-index`).** Same pruning idea as the decision tree above, but for the *other*
  first-match-wins dispatcher: `classify::classifier::classify_rules` (the flat `{when, value}` rule
  tables — `road.json`, topic `tag_rules`, `Producer::Classify`), not `categorize`'s category sets.
  Groups rules by their necessary `tag == value` condition (`necessary_tag_eq`: unwraps `and`/`or`
  when every branch agrees) into a `HashMap`, so only rules that could plausibly match a given
  `highway` value get `eval_filter`'d.
  - **Why not merged:** no measurable win on Brandenburg — `road.json` only has ~18 rules, so the
    `HashMap` lookup + group indirection costs about as much as the branches it skips. Byte-identical
    output confirmed; correctness isn't in question, just not worth the added indirection for tables
    this small.
  - **Trigger to revisit:** a rule table growing well past what `road.json` has today, the same way
    the tree's trigger is category-count growth.

- **Pass-C profiler breakdown (2026-07-09, Brandenburg, `PASS_C_PROFILE=1`) — no single bottleneck
  left; costs are spread across ~7 buckets.** Added `LIFECYCLE`/`TAGCLONE`/`EXCLUDE`/`PRECAT`/
  `SIDESPLIT` buckets to `profile.rs` (zero-cost when unset) to find out what's actually in the old
  "other" bucket this file used to blame on `lifecycle`. It isn't: `lifecycle` is ~2% of `classify`
  time. Roughly stable proportions across repeated runs: `sidesplit` ~18%, `extract` ~18%,
  `categorize` ~17%, unaccounted (Vec/HashMap allocation, loop overhead) ~19%, `tagclone`
  (`raw_tags.clone()` per topic per element) ~11%, `exclude` (`exclude_condition` eval) ~8%, `precat`
  (sidepath-self + `tag_rules`) ~6%.
  - **One real structural lead, investigated and rejected:** `tagclone`+`exclude`+`precat` (~25%
    combined) all run *before* `exclude_condition` is checked — `TopicRunner::process` clones the
    full tag map and runs `lifecycle` unconditionally, then `build_topic_rows` evaluates
    `exclude_condition` and bails. Reordering (check exclude on raw tags first, clone only if kept)
    was prototyped and measured: it does cut real work (`tagclone`'s share of `classify` roughly
    halved, `lifecycle`'s too), **but it silently changes output** — on Brandenburg it dropped 931
    `roads` rows + 113 `bikelanes` rows, all `highway=construction` + `construction=<allowed>` swaps
    and access-restricted-but-rescued ways that only survive today because `lifecycle` runs *before*
    `exclude_condition` (see `lifecycle.rs`'s construction swap and `remove_access_tags`). Reverted;
    not safe as a plain reorder.
    - **If ever revisited:** the real fix isn't reordering wholesale, it's a cheap raw-tag pre-check
      that mirrors just the few conditions `lifecycle` can change (raw `highway=construction` +
      `construction` value; raw `access=no`) — "could `lifecycle` rescue this way from exclusion?" —
      and only takes the fast (no-clone) exclude path when the answer is no. Fiddly, correctness-
      sensitive, not attempted.
  - **Conclusion:** no more free lunch here. Next real win (if ever needed) requires either the
    precheck above, or attacking the ~19% unaccounted allocation overhead directly (arena/reuse
    buffers across topics), not another dispatch-pruning trick.

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

- **Datafy the topic-specific transforms — push literals out of `.rs`, keep only true residue.**
  The goal is not "zero Rust" but "no topic literals in Rust": a Rust operator named + parametrized
  from JSON (like `split_sides` today) is already correct; the problem is bikelane/OSM vocabulary
  baked into `src/transform/*.rs`. Audit of the current pile against that test:
  - **Fully datafiable → delete the file:**
    - `cycleway_both.rs` (`cycleway=no` → `cycleway:both=no`) — all literal.
    - `opposite.rs` (`cycleway=opposite*` → explicit schema, 3 cases) — a value→{writes} case table.
    - `construction_prefix.rs` (strip `construction:` prefix, re-key, stamp lifecycle) — one wart: the
      `cycleway:/sidewalk:` lifecycle-key branch becomes a `stamp_key_by` param.
    - `lifecycle.rs`'s `highway=construction + construction∈allowed_highway` swap (already reads a
      value_set) and its `construction`/`baustelle`/`BLOCKED_TERMS` keyword lists (→ `value_sets.json`).
  - **True residue → stays native (behind a named interface):**
    - `side_split.rs` (structural center-line split; already `{highway,prefix}`-parametrized) — but
      lift its embedded `cycleway` unnest, `META_PREFIXES`, and directed-tag list into the
      `split_sides` JSON params.
    - `lifecycle.rs`'s `is_access_restricted` (the `highway=cycleway ∧ bicycle=no` / `footway ∧
      foot=no` access semantics + lowercase-concat free-text substring scan) — fuzzy heuristic.
    - `derive.rs::smoothness_parent` (re-runs a sibling Producer + provenance — engine orchestration).
    - `derive.rs::traffic_mode_side` — boundary case; native, or a Rhai `(tags)→value` script *only if*
      the bespoke-deriver set turns out to be open (see the Lua/Rhai deriver note — Rhai preferred:
      Rust-native, sandboxed, `Send` AST plays with rayon; Lua's `!Sync` state needs thread-local VMs).
  - **The 4 primitives that absorb the datafiable column** (a closed vocabulary, NOT a scripting lang):
    `rename_key` (value-gated rewrite), `value_cases` (tag value → set of writes/removes),
    `strip_prefix` (strip P, re-key, optional stamp), `keyword_scan` (lowercase-concat named fields,
    match a named term-set → set tag). After this, only 4 things stay topic-aware in Rust instead of 7+.
  - **Sequencing (cheapest first):** (1) `rename_key`+`value_cases` → delete `cycleway_both`+`opposite`
    + lifecycle swap; (2) `strip_prefix` → delete `construction_prefix`; (3) `keyword_scan` → shrink
    `lifecycle` to just `is_access_restricted`; (4) lift `side_split`'s literals into its JSON;
    (5) decide `traffic_mode_side`. Verify byte-identical output at each step.
  - **DONE (steps 1–2):** added `rename_key`/`value_cases`/`strip_prefix` primitives
    (`src/transform/mod.rs` `TagTransform` + `src/engine/topic.rs` `ParamTransform`); deleted
    `cycleway_both.rs`/`opposite.rs`/`construction_prefix.rs`; migrated `configs/tilda/bikelanes/topic.json`.
    Verified byte-identical on Berlin (`--output csv`, sorted-diff — raw row order is nondeterministic
    from parallel emit). **`lifecycle` left fully native** (not just `is_access_restricted`): its
    `highway=construction` swap can't be safely split off via `value_cases` because the branches share
    an early-return — extracting the swap while a reduced `lifecycle` still runs the access/keyword
    branches diverges on ways that are *both* construction-swapped *and* access-restricted (the original
    returns after the swap). So steps 3 + the lifecycle part of step 1 need `keyword_scan` to carry a
    guard condition + short-circuit, or stay native. Steps 4–5 still open.

- **Done, simpler than planned** (see CHANGELOG "Unreleased"): shipped as a one-time "all ways" pass
  reusing the existing pipeline/topic-config machinery (`configs/live_raw`, `accept_all` +
  `"outputs": true`) instead of a hand-rolled loader or `osm2pgsql` flex script — no new ingest code
  needed since `TopicRunner`/`Filter`/`Producer` were already tag-only and geometry-source-agnostic.
  `src/live_source.rs` is the bbox-query alternate input source (`--source postgis`) this item
  scoped as "~1 day, unchanged from before" — turned out closer to that estimate than the loading
  side, once loading was free. Left out of scope, as originally noted: the routing graph
  (`edges`/`nodes`), relations, and node-only topics — the live editor only ever needed way
  line-geometry preview.

- **Read auxiliary/annotation data into the live editor.** The `annotations jsonb` column that
  already exists on every tag table (`create_tag_table_sql`, `src/db/schema.rs`) is *not* user or
  editorial metadata — it's engine-internal bookkeeping the topic pipeline writes during tag
  transforms (e.g. `_side`/`_prefix` markers recording how a side-split/cloned feature was derived,
  readable via `Filter::AnnotationEq`). `src/live_source.rs`'s `fetch_ways()` doesn't even select it
  today — only `osm_id`, `produced`, and geometry/length. There is no existing user-facing
  annotation/notes/QA-flag table anywhere in the repo. To actually support auxiliary data (editorial
  notes, QA flags, custom attributes) added by users of the live editor, this needs building from
  scratch: a new table (e.g. `{table}_annotations(osm_id, ...)`), a join added to `fetch_ways()`,
  and threading the result through to the frontend/vite plugin (`editor/vite.config.ts`). Not
  started.

- **Promote the jsonb tag columns to named columns (generated per topic).** The four data columns
  (`osm`, `derived`, `private`, `meta`) are all flat `string→scalar` maps whose full key universe is
  statically knowable from each topic's JSON (`osm_fields` outputs, deriver keys + their const
  companions, `category`, private/meta keys). Provenance is *already* flattened — `runner.rs:34-35`
  emits companions as `<output>_<k>` (`surface_source`, `smoothness_confidence`), not nested — so
  nothing structurally forces jsonb.
  - **Win:** real per-column btree indexes (`WHERE surface='asphalt'` vs `derived->>'surface'`) for the
    tile server + downstream SQL; smaller storage (jsonb repeats every key string per row); and we
    delete the four per-row `serde_json::to_string` calls (`output/rows.rs:43-46`).
  - **Do it as a hybrid, not a flat explode:** promote the columns people filter/render — `osm_fields`
    outputs, derived *values*, `category` — to named typed columns; keep the `_source`/`_confidence`
    provenance companions, `private`, and `meta` as one `jsonb` column each (else a ~40-value table
    balloons past 100 columns of provenance nobody queries). Provenance stays promotable later.
  - **Machinery:** the DDL becomes per-topic and **generated from the loaded topic definition** (not
    hardcoded — same discipline as `value_sets`, so no topic literals in the binary; keeps
    the unbiased-engine principle). Today `create_tag_table_sql` is one topic-agnostic shape; this
    means enumerating the column universe per topic, generating `CREATE TABLE`, and threading a
    per-topic column list through the COPY/CSV writers (currently generic over one fixed `TAG_COLUMNS`).
  - **Low migration cost:** the pipeline truncates + rebuilds (full reimport), so JSON→schema coupling
    is handled by drop+recreate — no online `ALTER` concern.
  - **Composes with the relations→ways→nodes plan:** each per-kind topic table just generates its own
    column set.

- **Root `Dockerfile` (standalone one-container live-editor demo) is stale.** It bundles a single
  container with no Postgres service, but the live editor now requires `db`
  (`editor/docker-compose.yml`) plus a one-time "all ways" ingest pass — see the Next.js migration
  changelog entry. Needs either a Postgres service added to that image (`postgres` as a second
  process, or switch it to `docker compose`-based too) plus the ingest command run at build/start
  time, or retiring it in favor of `editor/docker-compose.yml` everywhere. Low urgency — it's a
  convenience demo image, not the primary dev path.
