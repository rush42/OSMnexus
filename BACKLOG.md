# Backlog

Deferred ideas / nice-to-haves for the Rust pipeline. Not blocking anything.

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

- **Live editor: load the base PBF into Postgres once, select bbox extracts from there instead of
  shelling out to `osmium extract` per drag.** Measured today: `osmium extract -s complete_ways`
  against a 115MB Berlin-region PBF costs ~2.2–2.5s per bbox change (two full passes over the whole
  file — PBF has no spatial index, so there's no way to skip the other 113MB); the pipeline run
  itself (`--output geojson` against the resulting ~2MB extract) is only ~0.15s. The extract step,
  not the pipeline, is the live-editor's actual latency.
  - **Easy path: use `osm2pgsql` flex output instead of a hand-rolled loader.** Rather than building
    a custom bulk loader + reverse node→way index, let `osm2pgsql`'s flex mode do the PBF parsing,
    way/relation assembly, and node-location middle — write a small Lua script that just dumps the
    raw shape: `nodes(id, tags jsonb, geom point)`, `ways(id, tags jsonb, node_ids bigint[], geom
    linestring)`, `relations(id, tags jsonb, member_ids bigint[])`. Flex's `way.nodes` gives the
    ordered node-id array directly in the callback, so there's no reverse index to build — a bbox
    query is just `ways WHERE geom && bbox` (GiST-indexed) plus a join back to `nodes` on
    `node_ids` for coordinates, which is exactly the `WayData`/`NodeData` shape
    `classify_way`/`classify_node`/`classify_relation` already take (decoupled from the PBF format).
  - **Estimate: ~2–3 focused days** (down from the hand-rolled estimate, since osm2pgsql absorbs the
    riskiest part):
    - Flex Lua script for the three raw tables + import against `berlin.osm.pbf` — ~0.5 day.
    - Bbox query (nodes/ways/relations in bbox, `complete_ways` node join) — ~0.5 day.
    - Wiring the query result into the pipeline as an alternate input source next to `stream_osm`
      (main.rs's producer currently assumes a PBF path) — still ~1 day, unchanged from before; this
      part doesn't get cheaper just because loading did.
    - Editor API integration + one-time "import base PBF via osm2pgsql" command, plus verifying
      output matches today's `osmium extract` path on the same bbox — ~0.5–1 day.
  - **Trigger to build:** only worth it if the ~2.2s/drag extract cost is still annoying after the
    cheaper mitigation (shrinking the base PBF to the region actually being edited, which should get
    extract time down to a few hundred ms for free). If that's enough, this is over-engineering for
    a local dev tool.

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
