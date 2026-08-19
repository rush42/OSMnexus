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

- **Tag/geometry CSV row order aligned by construction — DONE, incl. the point-table split
  (`{table}_node_geom`/`{table}_way_geom`) that removed the last mixing case. `parquet.rs` still
  not ported.** `geojson.rs` and Simon's parked GeoParquet PR (`simon/feat/geoparquet-output`,
  `src/output/parquet.rs`) join staged tag/geometry CSVs via a full `HashMap<i64, GeomRow>` keyed by
  `osm_id`. Investigating whether that could be a cheap positional join instead led through shard-
  keying (ruled out — `--output csv`/`geojson`/`geojsonseq` already runs `w=1`, so `Shard::send`'s
  round-robin was never the source of disorder there) and a sorted-merge-join design (superseded by
  what actually got built — see below) before landing on the real fix, in two commits:
  - **What was actually nondeterministic, and the fix (commits routing tag rows from the blob-order
    fold, then routing way geometry the same way):** `classify_way`/`classify_rel`/`classify_node`
    used to call `route_tag`/`route_member`/`route_node_point` as a side effect from inside
    `sorted.rs`'s parallel per-blob decode closures, racing across elements decoded concurrently
    within a chunk. They're now pure (return rows instead of routing them), and routing moved into
    the sequential, blob-order fold Pass A/B already used for their own accumulators
    (`FOLD_CHUNK_BLOBS`) — so a table's tag-row CSV order is now a deterministic function of blob
    order. Separately, `geom::materialize::run` used to resolve/route way geometry via
    `WayRefsStore`'s own MPHF-slot order (an id-derived order with no relation to blob order) — Pass
    A now also records `kept_way_order` (every tag-kept way's id, in the same blob-order fold that
    routes its tag row), and materialize walks that same order via a new
    `WayRefsStore::par_route_ordered` instead. Both fixes needed the same bounded-parallel-chunk +
    sequential-fold shape to avoid buffering a table's-worth of rows in memory at once — and both hit
    the same lesson tuning chunk size (see below).
  - **Result: no sort needed at all.** Since tag and geometry rows are now written in matching
    relative order *as they're produced*, the downstream join can become a plain positional zip (two
    file readers advanced in lockstep) — cheaper than the sorted-merge-join idea this entry used to
    describe, which would have needed an `O(n log n)` sort pass first. Verified on `berlin.osm.pbf`:
    `roads.csv` <-> `roads_geom.csv` match `osm_id` at every row position (0 mismatches / 391828
    rows); `bikelanes`' side-split tag rows (multiple per way) match geometry order once deduped.
    Byte-identical across repeated runs.
  - **RSS lesson (bit twice, same root cause):** buffering a whole `FOLD_CHUNK_BLOBS=256`-blob chunk's
    tag rows (not just cheap ref/id data) before routing blew up peak RSS +80% on
    `brandenburg-latest.osm.pbf` (550MB → 990MB) for no measurable time benefit — shrunk to `16`,
    matching baseline RSS with no slowdown. `way_refs.rs`'s new `ROUTE_CHUNK_WAYS` was sized `512`
    conservatively from the start given that lesson, and benchmarked clean (no measurable RSS/time
    regression on the same extract). Any future chunk-size tuning on either constant should re-run
    this same before/after `/usr/bin/time -v` comparison — the payload held in the transient, not the
    blob/way count alone, is what determines the RSS cost.
  - **Still N/A for the PostGIS/COPY path.** `copy_writer` (`writers.rs:29-56`) streams straight into
    per-table `COPY ... FORMAT BINARY`; there is no in-process join there at all — cross-table
    correspondence is handled by Postgres's own indexed `JOIN` at query time, so none of this applies.
  - **DONE for GeoJSON/GeoJSONSeq way geometry.** `geojson.rs`'s `build_features` now reads
    `{table}_geom.csv`/`{table}_polygon.csv` via `OrderedWayGeomCursor` — a forward cursor consumed
    in step with `{table}.csv`'s own way rows (peek, consume-if-match, hold-for-later otherwise) —
    instead of a `HashMap<i64, Geom>` + `.get(&osm_id)`. `read_way_geom` (the old hashmap reader) is
    gone. Verified byte-identical GeoJSON/GeoJSONSeq output against the pre-change build on
    `berlin.osm.pbf` (tilda: roads, bikelanes) and `public_transport` config.
  - **DONE for relations too.** `RelMembers` had the exact same problem `WayRefsStore` did:
    `RelMembers::build` fed the relations pass's blob-ordered fold into the same `MphfArena` that
    discards insertion order (sorted by hashed slot — `store.rs`'s `build`), so
    `geom::materialize::relations()` (via `RelMembers::requests()`, `self.0.iter()`) built relation
    geometry in MPHF-slot order, unrelated to the relations pass's blob order. Fixed the same way:
    `classify_relations` now also returns `kept_relation_order`, threaded through
    `SelectionContext`, and `RelMembers::requests_ordered(&self, order)` replaces `requests()` —
    sequential, not chunked/parallel like the way case, since relation counts are orders of magnitude
    smaller than way counts. `geojson.rs` gained `OrderedRelationLineCursor`/`OrderedPointCursor` for
    `{table}_relation_geom.csv`/`{table}_relation_point.csv` (relation polygon reuses
    `OrderedWayGeomCursor` — same column shape); `read_relation_line_geom`/`read_polygon_geom` (the
    old hashmap readers) are gone. Verified on `berlin.osm.pbf`/`configs/trains` (the one shipped
    config with relation line geometry): 106/106 relation ids match position; byte-identical GeoJSON
    against the pre-change build there, on `public_transport` (graph-fallback path, still hashmap via
    `edges.csv`), and on `tilda` (way-only sanity check).
  - **Point-table mixing — DONE, via a config schema change, not a cursor workaround.** The one
    remaining `HashMap` was `{table}_point.csv`, which shared its channel between node-point rows
    (select phase, node order) and way-point rows (materialize phase, way order) — an `osm_id`
    sequence `[node points][way points]` with no relation to `{table}.csv`'s own R/W/N block order,
    so a forward cursor there would've silently attributed zero points to every way. Root-caused to
    geometry tables being organized by *shape* (line/point/polygon) instead of *element kind*
    (node/way/relation) — the one shared "point" table was the only place two different kinds'
    output landed in one file. Fixed by reorganizing `topic.json`'s geometry schema itself: each
    kind now gets exactly one declared shape (`"geometry_output": { "way": "line" }`, not a list —
    every shipped config only ever declared one anyway) and its own physically separate table
    (`{table}_node_geom`/`{table}_way_geom`/`{table}_relation_geom`, self-describing via a
    `geom_type` column). No more shared point table, so no more mixing — `geojson.rs` now uses the
    same `OrderedGeomCursor` for all three kinds. See CHANGELOG's "Unreleased" entry for the full
    schema-change writeup (also collapses tag-table-adjacent geometry tables from up to 6 per topic
    to 3, and separates the routing-graph flag into its own orthogonal `"graph"` field). Verified
    byte-identical output against the pre-change build on `berlin.osm.pbf` (tilda/trains/live_raw)
    and `brandenburg-latest.osm.pbf` (tilda); no RSS/time regression on the same benchmark used for
    the way/relation alignment work above.
  - **Still `HashMap`-based, and expected to stay that way:** `edges.csv` (the relation path needs
    random access into it by arbitrary member-way id, and no shipped config exercises the
    way-graph-fallback path that would benefit anyway — see the graph-topics note above).
    `parquet.rs` (Simon's parked PR) hasn't been touched at all yet — same cursor rewrite (now even
    simpler, given the one-table-per-kind schema) would apply there once that branch is picked back
    up.

- **`categorize::linter::tests::categories_are_disjoint` (and its underlying `find_overlaps`) hangs
  in practice — an algorithmic complexity bug, not a deadlock.** Confirmed via instrumentation:
  `to_dnf` (`src/categorize/linter.rs:218`) does the textbook exponential DNF blowup for `And`-of-`Or`
  filter conditions — several `configs/tilda/bikelanes` categories alone produce 1,000–6,400 raw DNF
  terms each from modestly-sized JSON filter trees (e.g. `foot_and_cycleway_shared_adjoining` → 6,426
  terms). `find_overlaps`'s pairwise loop (`src/categorize/linter.rs:367`) then does an
  `O(terms_a × terms_b)` nested scan via `check_term_consistency` for every one of `O(n²)` category
  pairs per topic — with per-category term counts in the thousands, this multiplies into billions of
  calls. Reproduced single-threaded, in isolation (`--test-threads=1`, exact test name), on
  unmodified `main` — not related to any in-flight branch work. The `bikelanes` topic (26 categories)
  finished DNF-precompute for all 26 within 20s but never got past its own pairwise phase in that
  window. Needs a real fix (memoize/short-circuit `check_term_consistency`, cap DNF term explosion,
  or avoid full DNF materialization entirely) — not attempted yet, flagged as its own task.

- **Root `Dockerfile` (standalone one-container live-editor demo) is stale.** It bundles a single
  container with no Postgres service, but the live editor now requires `db`
  (`editor/docker-compose.yml`) plus a one-time "all ways" ingest pass — see the Next.js migration
  changelog entry. Needs either a Postgres service added to that image (`postgres` as a second
  process, or switch it to `docker compose`-based too) plus the ingest command run at build/start
  time, or retiring it in favor of `editor/docker-compose.yml` everywhere. Low urgency — it's a
  convenience demo image, not the primary dev path.

- **`produced`/`annotations` are parsed and reprinted on every GeoJSON feature — NOT BUILT.**
  `TopicRow::produced`/`annotations` are already JSON *text*, pre-serialized on the rayon classify
  workers (that's deliberate — see the field's own doc for why it isn't left as a `Map`).
  `output::geojson::merge_properties` then does `serde_json::from_str` into a `Value` tree, extends
  it into the feature's property map, and `to_writer` prints it straight back out. So the bulk of
  every feature's payload is parsed and reprinted for nothing.
  - **Shape.** Enable serde_json's `raw_value` feature (`Cargo.toml` currently has a bare
    `serde_json = "1"`) and parse into `BTreeMap<String, &RawValue>` instead: keys still get parsed,
    which is required — they carry the sort order and the later-wins dedup that `Map::extend`
    provides today — while values stay borrowed slices of the original text, never parsed, never
    reprinted.
  - **Sizing.** After the `Serialize`-instead-of-`Value` change, germany's geojsonseq join is 35.0s
    against `parquet`'s 13.4s on identical cursors and identical data. That ~21.6s gap is text-vs-WKB
    plus this round trip; this entry targets the second half of it. Coordinates are already handled.
  - **Hazards.**
    - Byte-identity holds only if the stored text is already in serde_json's canonical form. It
      should be — serde_json wrote it — but escapes and float formatting are exactly where that
      assumption breaks quietly. Verify with the bremen gate, don't assume it.
    - `merge_properties` today silently ignores both a parse failure and a non-object value
      (`if let Ok(Value::Object(map))`). Preserving that leniency is a behaviour requirement, not an
      oversight to clean up.
    - `base_properties` mixes in `osm_id`/`id`/`category` as real `Value`s. A `&RawValue` map needs
      those unified — either a small enum, or emit the merged map by hand in sorted key order.

- **Sharded intermediate staging files, for a parallel join — NOT BUILT.** The join is currently one
  worker walking one `{table}.bin` against the per-kind geometry files. Splitting a *single* staged
  file across workers doesn't work: geometry is a subset of tags (bremen: 7.1 MB against 11.9 MB,
  different row counts), so a frame boundary in the tag file says nothing about where to cut the
  geometry file — aligning them needs an actual key search. Sharding removes that by construction:
  shard *i*'s tag file and shard *i*'s geometry files cover the same element set in matching order.
  - **Partition key is the real decision.** `osm_id % S` balances near-perfectly but reorders the
    output, which breaks the byte-identity gate every change on this branch has been verified with.
    Contiguous *blob ranges* preserve output order exactly (concatenating shards in shard order
    reproduces today's sequence) and the blob index is already built up front, at some risk of skew
    from uneven kept-element density. Try blob ranges first; measure the skew before paying for
    hashing. Round-robin is simply wrong here — it puts a way's tag row and its geometry in
    different shards.
  - **All three element kinds need their own geometry file per shard; they cannot be merged.** Tag
    rows are emitted relations → ways → nodes (`osm::reader::sorted`'s own module doc), while
    geometry is written nodes (Pass B) → ways (materialize) → relations (after materialize,
    `main.rs:400`). That is the *exact reverse*, so every pairing (N+W, W+R, N+R) is out of order
    against the tag stream and no forward cursor can serve it. The three-way split from `68c77604f`
    is forced, not incidental — its own commit message only had evidence for the N/W case.
  - **Note.** `edges` is dual-use: `EdgeCursor` walks it in lockstep (shards fine, keyed by
    `osm_id`), but `group_edges_by_way` needs a map across *all* shards for the relation fallback.
    File count also grows to 4 per shard per topic — S=32 with 2 topics is ~256 staging files.

- **In-memory staging, auto-selected — NOT BUILT.** `sinks::memory_sink` exists, is documented, and
  is dead code: nothing calls it, and there is no `Output::Memory`. The write half is easy (file
  backends already run `w = 1`, so a single shard collects in routing order). The read half is the
  work, and it is now cheap because everything funnels through `next_row() -> Result<Option<R>>`:
  add `RowSource<R>`, impl it for `StageReader<R>` and `vec::IntoIter<R>`, and make
  `OrderedGeomCursor` / `for_each_feature` / `write_table_parquet` generic over it. Before the
  struct-contract change there was no such seam — the read side was `read_binary_row(&mut reader,
  &schema)`, a decode welded to a byte stream plus a Postgres schema vec.
  - **Don't select on input file size.** Staged volume tracks the *config*, not the input: bremen is
    1.28× its PBF, germany ~1.1×, both on `configs/tilda`, and a config that keeps more moves that
    a lot. Live structs also cost more than their staged bytes. Gate on available RAM and carry a
    **byte budget with spill-to-disk** — then the heuristic only has to be safe, not right. Guessing
    "disk" wrongly costs a little speed; guessing "memory" wrongly OOMs after minutes of work.
    `RowSource` makes the spill nearly free (chained iterator: these rows, then this file).
  - **Sizing.** germany stages ~10.2 GB of `.bin`. This is an extract-scale / live-editor feature;
    the file path stays the default.

- **Stream the join during materialize — NOT BUILT.** Tag rows all exist before materialize starts,
  so as each way geometry is produced in `kept_way_order` the tag cursor can advance and emit that
  feature immediately; node features can stream during select. **Relations can't** — a relation's
  graph-fallback geometry pools edges of arbitrary member ways (`group_edges_by_way`, keyed by
  non-adjacent ids), so it needs every edge first, and relation geometry is resolved after
  materialize anyway. Relations stay a post-pass.
  - **The prize is peak memory, not wall-clock.** You stop holding tag rows *and* geometry rows at
    once — on bremen's roads pair, geometry is 7.1 MB of 19.0 MB, so ~37% off. The floor you can't
    stream away is the tag rows themselves (63% of that pair), which are produced a phase earlier
    than the geometry they join to.
  - **Unverified assumption.** The claim that this buys little wall-clock rests on materialize
    already saturating its threads. That has not been measured. If materialize has idle threads or a
    serial tail, the join genuinely hides in it and the win is larger than assumed. Measure before
    designing around either answer.

- **The graph-fallback feature path has no coverage at all.** No shipped config declares `"graph"`
  (`grep` finds it nowhere in `configs/`), so `emit_graph_fallback_features`, the `seg_idx` segment
  features, and the `"cut"`/`"endpoint"` marker features never execute — not in the test suite, and
  not under the bremen byte-identity gate that every output change on this branch has been verified
  with. `68c77604f` noted this in passing; it is worth stating as its own gap, because it means that
  code is otherwise changed on inspection alone.
  - **Partly closed.** `output::geojson`'s unit tests now cover `emit_graph_fallback_features`
    directly (per-segment `seg_idx`, no leakage between segments, an existing `seg_idx` being
    overwritten rather than duplicated, empty input, and the marker key ordering `kind` < `osm_id`),
    added alongside the shared-buffer rewrite of that function so the change was verifiable at all.
  - **Still open:** nothing covers `for_each_feature`'s *integration* over real staged files for a
    graph topic — the cursor interplay between `EdgeCursor::get_all` and the tag stream, and the
    cut/endpoint interleaving. That needs a fixture config declaring `"graph"`, which no shipped
    config does. Until then the byte-identity gate still cannot see this path.

- **`inherit_to_member`'s parent read is UNVERIFIED end to end.** The mechanism is built: a member
  way's parent relation is exposed as `ExtractCtx::parent_tags`, fanned out *before* categorization
  so each row is categorized and evaluated against its own parent, and a way topic is meant to read
  it with the existing `{"parent": {"key": ..}}` / `parent_or_obj` producer syntax. Fan-out itself is
  confirmed on bremen (way 132726057 emitted 16 times, one per route, ids suffixed
  `/relation/<id>`), and `configs/tilda` stays byte-identical.
  - **What is not confirmed** is that a way producer actually reads a value out of that parent. The
    scratch config built to test it could not get *any* topic-level producer to emit — a plain
    object-tag probe (`{"match": [{"key": "railway"}]}`) was absent from `produced` too, so the
    failure is in how that config declares producers, not necessarily in the parent plumbing. Both
    readings are still open.
  - **Cheapest way to settle it**: a unit test over `build_topic_rows` with a hand-built
    `TopicRunner` whose producers read `{"parent": ...}`, rather than another config round trip.
    That also gives the feature the regression cover it currently lacks entirely.

- **Node point coverage in `configs/public_transport` is 93 of 1,994 — unexplained.** Declaring
  `"geometry_output": { "node": "point" }` alongside the relation-line fix yields 93 Point features
  against 1,994 node tag rows (4.7%), on bremen. The relation half of the same declaration covers
  665 of 665, so this is specific to nodes. Two readings, and they have very different consequences:
  - most of those node rows are for elements the point routing deliberately skips (a config gap,
    harmless); or
  - `OrderedGeomCursor` is losing alignment on the node arm. Node geometry is written during the
    select phase and joined positionally against the tag stream, so a misalignment would silently
    drop features **for every file backend and every config with node geometry**, not just this one.
  The `node: point` declaration was deliberately left out of the shipped fix until this is settled —
  it is cheap to check (compare `{table}_node_geom.bin` row count against the tag file's `N` rows)
  and should be checked before anyone adds it.
