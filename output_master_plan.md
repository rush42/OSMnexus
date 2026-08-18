# Output redesign — master implementation plan

Implementation plan derived from [`output_plan.md`](output_plan.md), after a verification pass over
the code it describes and a fresh `--threads` sweep on `germany-latest.osm.pbf`.

`output_plan.md` stays the design record — why sort/merge over hashmap, why in-memory staging is
opt-in, what `as_member` is for. This file is the *execution* order: what to build, in what
sequence, and which measurement gates decide whether the later phases happen at all.

**Headline conclusion, after measuring:** neither the sort/merge redesign *nor* the ordered-fold
parallelization is worth building. The measured ordered tax at usable thread counts is **+4-7%**
(§2c), of which the fold — Phase 2's entire target — is about **3 seconds**. What actually cost time
was something in neither plan: **fixed-size fork/join barriers** in the chunk constants. Sizing those
against the rayon pool took the best germany run from 147.6 s to 136.1 s and cut materialize by up to
50%, with no memory cost (§2b).

Two things remain worth doing, both output-side rather than pipeline-side:

- **the join** — ~26 s, flat at every thread count, ~19% of the run and larger than the whole ordered
  tax nearly everywhere (Phase 4a, then 4c);
- **`par_route_ordered`'s arena access** — +13.8 s at 56 threads and the only ordered cost that
  *grows* with threads (§2c).

`w > 1` for file outputs is reached by **hash-partitioning on `osm_id` instead of round-robin**
(§1.4), which preserves the correspondence the forward cursor already relies on. The sort, the k-way
merge and the shard-framing work are all unnecessary under this plan.

The original plan's 57% ordered-tax figure was not wrong so much as **thread-count-specific** — it was
taken at `--threads 8`, where our own isolated A/B also shows +29%. At 16-32 threads the same tax is
+4-7%. Generalising that one measurement is what pointed both plans at the fold.

---

## 1. Verification pass — corrections to `output_plan.md`

Four findings that change the plan. Each was read out of the code, not inferred from the doc.

### 1.1 The Pass A diagnosis is imprecise, and the imprecision matters

`output_plan.md` states the ordered tax comes from funnelling *"every blob's accumulator work
(`way_refs`/`counts`/`kept_way_order` in Pass A)"* through one thread per chunk.

Reading `classify_and_index` (`src/osm/reader/sorted.rs:207-300`), the accumulator fold over
`per_blob` — `counts`, `present`, `way_refs`, `extra_node_ids` — is **sequential in both modes**
(`sorted.rs:277`). It is not gated on `ordered` at all. The only `ordered`-conditional work is:

- carrying `topic_rows` out of the parallel closure inside `ClassifiedWay`,
- the sequential `merge_topic_rows` loop over the whole chunk (`sorted.rs:266`),
- `kept_way_order.push` (one `i64` per kept way).

So the Pass A tax is **tag-row marshalling, not accumulator folding**. `way_refs`/`counts` are a
sequential cost the unordered path pays too, and no join redesign would remove them.

This is good news: the expensive part is local and has a clean parallel form.

### 1.2 A cheap, order-preserving fix exists that was never scoped

`per_blob` is already a `Vec` indexed by blob position, and the unordered path already builds a
per-blob `tag_batch` **inside the parallel closure** (`sorted.rs:238`). Ordered mode can do exactly
the same and then route those per-blob batches sequentially in blob order:

```
parallel:   each blob builds its own tag_batch      (O(rows) work, now parallel)
sequential: for batch in per_blob_batches { route_tag(batch) }   (64 sends per chunk)
```

Row order is unchanged — blob order *is* the ordering key, and appending blob 0's rows then blob
1's rows to a shard yields the same sequence as merging them in blob order and sending once. The
O(rows) merge work leaves the critical path; the sequential residue is 64 cheap sends per chunk.

This is the *"concatenating per-blob results in already-known order"* alternative listed under
**"Not yet decided against"** in `output_plan.md` and never scoped out. It appears to be a small,
contained change to one function.

### 1.3 Fixed chunk constants are an independent scaling ceiling

`FOLD_CHUNK_BLOBS = 64` (`sorted.rs:134`) and `ROUTE_CHUNK_WAYS = 512` (`way_refs.rs:115`) impose a
rayon fork/join barrier per chunk. At 56 threads that is ~1 blob and ~9 ways of work per thread per
barrier — the barrier dominates the work it guards.

The sweep below measures exactly this: **Materialize gets *worse* as threads increase**, 35.4 s at
16 threads → 61.1 s at 56. That is not the join design; it is barrier overhead on a fixed chunk
size. Both constants need to scale with the rayon pool, and that fix is independent of every other
phase here.

Note both constants were tuned for **peak RSS**, not throughput (`sorted.rs:125-133` records the
256 → 64 cut after an +80% RSS regression on brandenburg). Any change must re-check RSS, not just
wall time.

### 1.4 File outputs are pinned to one writer shard — and this constrains the phase order

`main.rs:155-183` sets `w = 1` for `csv`/`geojson`/`geojsonseq`/`parquet`; only `pg` gets
`--db-writers`. So one writer task per table does all binary encoding, behind one mutex
(`sinks.rs:93`).

This is not an oversight, and **it cannot simply be raised**: `Shard::send` round-robins across
senders (`let kk = self.rr.fetch_add(1, Ordering::Relaxed) % w`, `sinks.rs:93`). With `w > 1`, rows
for one table are split across independent files in round-robin order — which destroys the
position-matched order the cursor join depends on.

**Therefore `w > 1` for file outputs requires changing how rows are partitioned — but not
necessarily by abandoning order.** Round-robin is the problem, not sharding itself. Partition both
the tag and geometry tables on `h(osm_id) % S` and corresponding rows land in the *same* shard, with
relative order preserved inside it (both are the same ordered sequence filtered by the same
predicate — filtering preserves subset-plus-relative-order, exactly the invariant the cursor
requires). `OrderedGeomCursor` then works per shard, unchanged.

This is a real dependency edge `output_plan.md` does not record, and it reframes the sort/merge
proposal rather than supporting it: sort/merge exists to survive *unordered* routing, but
hash-partitioning keeps the correspondence the cursor join already relies on while still handing
every shard to a different core. The output side can scale without it. See Phase 4.

---

## 2. Fresh measurement: `--threads` sweep on germany

`germany-latest.osm.pbf` (4.82 GB, 2026-08-17), `--config-dir configs/tilda --output parquet`
(i.e. `ordered = true`), exclusive Slurm node `head007` (2× Xeon Gold 6326, 64 threads, 503 GB),
PBF and output on node-local NVMe, page cache re-warmed before every run. Binary built at
`7e46b7022`. **Mean of 2 reps** (per-rep spread noted below):

| threads | total | Select (Pass A + B) | Materialize | Parquet write | peak RSS |
|--------:|------:|--------------------:|------------:|--------------:|---------:|
| 1  | 566.1 s | 391.9 s | 148.5 s | 25.6 s | 7.9 GB |
| 2  | 405.5 s | 273.5 s | 106.0 s | 25.8 s | 7.9 GB |
| 4  | 268.5 s | 163.0 s |  78.7 s | 26.6 s | 7.9 GB |
| 8  | 194.4 s | 113.5 s |  54.6 s | 26.1 s | 7.9 GB |
| 16 | 152.8 s |  90.3 s |  **35.4 s** | 26.9 s | 8.0 GB |
| 32 | **147.6 s** |  78.3 s |  42.4 s | 26.7 s | 8.1 GB |
| 56 | 164.3 s |  **75.6 s** |  61.1 s | 27.5 s | 8.3 GB |

Both reps agree on every conclusion below. Reproducibility is excellent at the serial end (t=1:
566.3 / 565.9 s, 0.07% apart) and looser in the middle (t=4 spread 6.0%, t=2 2.4%) — itself a
finding: **low thread counts need more reps than high ones**, which is why the Phase 0 harness
keeps every rep instead of averaging them away.

What this establishes:

0. **The whole pipeline returns 3.83× for 32× the threads** (566.1 s at 1 → 147.6 s at 32) —
   **12% parallel efficiency at its own optimum**. This single number is the case for the work
   below: on a 64-thread machine the ordered path leaves most of the box idle.
1. **The ordered path stops scaling at ~16–32 threads and then regresses.** Best total is 32
   threads; 56 is *slower* than 16. Anyone running tilda on germany today should use
   `--threads 16` or `32`, not `0`/all-cores.
2. **Select scales, sublinearly and with a hard ceiling**: 391.9 → 75.6 s across the full 56×
   thread range, i.e. ~5.2×. Consistent with §1.1/§1.2 — real parallel work exists, but tag
   marshalling is serialized behind it.
3. **Materialize is a clean U-curve with its minimum at 16 threads**: it scales 4.2× from 1 → 16
   threads (148.5 → 35.4 s), then *reverses* and gives most of that back by 56 (61.1 s, +73%). The
   U reproduced in both reps independently (34.6→60.3 and 36.2→61.8). A phase that improves and then
   degrades on the same workload is the signature of fixed-size fork/join barriers, not of the join
   design — exactly §1.3's `ROUTE_CHUNK_WAYS` analysis, and the single most actionable number here.
4. **Parquet write is flat at 25.6–27.5 s across the entire 1 → 56 thread range** — the `w = 1`
   single-writer cost of §1.4, and about as clean a constant as this kind of measurement produces.
   It is ~18% of the best run and no thread count touches it — Phase 4 is what makes it scale.
5. **Memory is a non-issue at this scale**: ~8 GB peak, flat across all thread counts, on a 64 GB
   budget. The bounded-memory guarantee that makes in-memory staging risky (§5) has a lot more
   headroom on germany than `output_plan.md` assumed.

Caveats, stated plainly: two reps; one hardware config; `--output parquet`
rather than the doc's `--output pg`, so these numbers are **not** comparable to the doc's
236.8 s / 371.8 s ordered-vs-unordered table and do not independently confirm the 57% figure. What
they do establish is the *shape* of ordered-mode thread scaling, which the doc never measured.

### 2b. Re-measured after Phase 1 (same machine, same flags, 2 reps)

`--output parquet`, head007 exclusive, binary built from this branch:

| threads | total | vs baseline | materialize | vs baseline | join |
|--------:|------:|------------:|------------:|------------:|-----:|
| 1  | 566.7 s | +0.1% | 145.8 s | −1.9% | 25.6 s |
| 2  | 410.8 s | +1.3% | 109.5 s | +3.3% | 25.4 s |
| 4  | 272.9 s | +1.6% |  79.0 s | +0.4% | 25.9 s |
| 8  | 184.3 s | −5.2% |  41.5 s | **−23.9%** | 26.7 s |
| 16 | 142.9 s | −6.5% |  26.1 s | **−26.1%** | 26.3 s |
| 32 | **136.1 s** | −7.8% |  26.5 s | **−37.5%** | 26.8 s |
| 56 | 136.8 s | **−16.8%** |  30.8 s | **−49.6%** | 26.0 s |

t=1/2/4 are controls — neither constant changes there — and they drift +0.1% to +1.6%, which is the
noise floor. Everything from t=8 up is real. Peak RSS unchanged throughout.

The Materialize U-curve is gone: it no longer degrades from 16 → 56 threads. Best total is t=32/t=56
**tied within noise** (136.1 / 136.8), where the baseline's best was 147.6 s at t=32 and 56 was
actively worse. So all-cores is now safe; it is not measurably *better*.

### 2c. The ordered tax, isolated (this is what decides Phase 2)

`--output csv` both arms, `OSMNEXUS_FORCE_ORDERED` the **only** variable — backend, encoding, writer,
machine and data all held fixed. This removes the confound in every previous ordered-vs-unordered
number, including `output_plan.md`'s. head010 exclusive, 2 reps, arms run back-to-back per thread
count so drift hits both equally:

| threads | unordered | ordered | tax | **select Δ** | materialize Δ |
|--------:|----------:|--------:|----:|-------------:|--------------:|
| 1  | 397.5 s | 533.0 s | +34.1% | +41.9 s | +93.6 s |
| 8  | 128.8 s | 166.1 s | +28.9% | +19.4 s | +18.3 s |
| 16 | 114.0 s | 121.5 s | **+6.6%** | **+3.4 s** | +4.6 s |
| 32 | 102.6 s | 107.1 s | **+4.4%** | **−1.5 s** | +6.4 s |
| 56 |  94.9 s | 111.4 s | +17.4% | **+3.3 s** | +13.8 s |

Three things follow.

1. **The ordered tax is strongly thread-count-dependent**, from +34% at one thread to +4-7% at
   16-32. `output_plan.md`'s headline figure was measured at `--threads 8`, which sits squarely in
   the high-tax regime — our own t=8 arm shows +28.9%. The magnitudes differ (that measurement was
   `pg`, with database ingest in the loop, and reported +57%), so this is not an exact
   reconciliation, but the direction and the thread regime match. Generalising a t=8 measurement is
   what pointed the original plan — and mine — at the fold.
2. **Select Δ is Phase 2's entire addressable surface, and it is ~3 s at t≥16** — negative at t=32,
   i.e. indistinguishable from noise. See Phase 2 below.
3. **Materialize Δ is the only ordered cost that *grows* with threads** (+4.6 → +6.4 → +13.8 s).
   That is `par_route_ordered` probing the MPHF arena in `kept_way_order` against `par_route_all`'s
   sequential scan — a memory-access difference, not a synchronisation one. It appears in neither
   plan and is now the second-largest addressable item.

---

## 3. Open decisions — resolved or sharpened

| # | `output_plan.md` question | Resolution |
|---|---|---|
| 1 | Sort/merge vs parallelizing the ordered fold | **Measured: neither.** §2c puts the fold's share at ~3 s and the whole ordered tax at +4-7% at t≥16, so Phase 2 is not worth building and unordered-always has little left to win. Reach `w > 1` by hash-partitioning (Phase 4c), which *preserves* the cursor join rather than replacing it. Sort/merge is retired to a contingency. |
| 2 | `--join-shards` default | **Deferred to Phase 4** — still needs tuning data, and Phase 4 may not happen. Do not pick a number now. |
| 3 | Does in-memory staging supersede sort/merge? | **No — keep independent.** `geojsonseq` streams today and would lose that property; sort/merge is the only answer that serves all three backends. In-memory becomes an opt-in fast path (Phase 5), never the only path. |
| 4 | Does `--output csv` need `ordered`? | **Resolved: it does not.** There is no `output/csv.rs`, and `main.rs:398-425` branches a post-run join only for `GeoJson`/`GeoJsonSeq`/`Parquet`. CSV writes independent per-table files with no cross-file correlation. One-line fix, Phase 1. |
| 5 | `as_member`-for-edges | **Out of scope for this plan.** Separate track (Phase 6), owns a flagged `public_transport` regression, and blocks nothing here. |

---

## 4. Phased plan

Each phase is independently shippable and independently revertible. Gates are measurements, not
opinions.

### Phase 0 — Measurement harness (prerequisite)

Turn the ad-hoc sweep into a committed, repeatable benchmark, because every later gate depends on
trustworthy numbers, and `output_plan.md` already records one wrong conclusion caused by
unmeasured background load.

- Script that runs a matrix of `{output backend} × {threads}` and emits a TSV of
  wall/Select/PassA/PassB/Materialize/join/peak-RSS.
- Must assert an idle machine (exclusive allocation or a load check) and re-warm page cache per run.
- Record the binary's commit hash in the output.

*Exit:* a rerunnable command that reproduces §2's table within noise.

### Phase 1 — Free wins (no design risk)

1. **[DONE] `--output csv` → `ordered = false`** (`main.rs:196`). Per decision #4. Verified by
   running the same binary twice on bremen with and without `OSMNEXUS_FORCE_ORDERED=1`: raw row
   order differs (so the path really changed) while content is identical as a set on every table.
   Also makes csv eligible for `w > 1`, since it has no join to break.
2. **[DONE] `ROUTE_CHUNK_WAYS` → thread-relative** (`way_refs.rs`, now `route_chunk_ways()` =
   `max(512, threads × 128)`). This is the constant that governs the Materialize U-curve, and its
   transient (resolved geometry) is unrelated to the `topic_rows` transient that caused the historic
   RSS blowup. Verified order-neutral: staged `.bin` and final `.parquet` are byte-identical between
   chunk sizes 512 and 1024 on bremen.
3. **[DONE, unordered half] `FOLD_CHUNK_BLOBS` → `fold_chunk_blobs(ordered)`.** Split, because the
   two constants are *not* equally safe to raise. The RSS ceiling that pinned this at 64 is
   **`ordered`-only**: the row payload crosses the fold boundary purely to reach the sequential
   `route_tag`, and when `!ordered` every pass routes from inside its own parallel closure and stores
   nothing (`ClassifiedWay.topic_rows` is `Vec::new()`, `classify_relations` pushes `None`,
   `collect_coords` never fills `classified`). So unordered scales by pool size now
   (`max(64, threads × 4)`), and `ordered` stays at the floor until Phase 2 removes that transient.

   This is **the only item in the plan that benefits `--output pg`** (see §5), and for pg it carries
   no RSS risk at all. Applies to all three passes — relations, Pass A, Pass B — which are all
   unconditional chunk loops.

   *Verified:* ordered output byte-identical (the path is provably untouched); unordered
   multiset-equal, with the byte-level difference shown to be pre-existing — the **old** binary
   differs from itself run-to-run on identical flags, because unordered `route_tag` fires from the
   parallel closure.
4. **[WITHDRAWN] Raise `w` for `--output csv`.** Not merely order-unsafe — *unimplemented*.
   `Shard::spawn` hands every one of the `w` writers the same path (`out_dir.join("{table}.csv")`),
   so `w > 1` would race N writers onto one file. Enabling it means shard-indexed filenames and
   changes csv from one file to N per table. Since csv has no join step at all (measured
   `join_s = 0.0`), the only gain is writer-side encoding — not worth the contract change. Keep
   csv at `w = 1`.

*Exit gate:* re-run Phase 0. Expect the 56-thread Materialize regression to shrink or vanish. If
the chunk change moves peak RSS materially, lower `PER_THREAD` and record the tradeoff.

### Phase 2 — Parallelize the ordered fold *(MEASURED, NOT WORTH BUILDING)*

**Do not build this.** §2c measures Phase 2's entire addressable surface — the Select delta between
ordered and unordered — at **+3.4 s (t=16), −1.5 s (t=32), +3.3 s (t=56)**, i.e. under ~3% of the run
and negative at one point. The fold serialization was largely a *symptom* of chunks being too small,
and Phase 1.2 absorbed it.

This was the headline recommendation of this plan, and it was wrong. It was reasoned from
`output_plan.md`'s 57% figure plus my own sweep, both of which measured a large ordered tax — but the
former was taken at `--threads 8` (high-tax regime) and the latter was confounded by the fixed
chunk barrier. Once the barrier was fixed and the confound removed, the fold's share was ~3 s.

Kept below for the record, and because the *reasoning* remains valid if the ordered tax ever grows
again (e.g. if `route_tag`'s cost rises, or a config produces far more tag rows per way than tilda).

Scope would have been one function, `classify_and_index` (`sorted.rs:207-300`):

- Have each blob's parallel closure build its own `tag_batch` (mirroring the existing `!ordered`
  path at `sorted.rs:238`), returning it alongside its `ClassifiedWay`s.
- Replace the sequential `merge_topic_rows` loop (`sorted.rs:266`) with an in-blob-order walk that
  just calls `route_tag` per blob.
- Delete `topic_rows` from `ClassifiedWay` — it exists only to ferry rows into the fold.

Apply the same treatment to `par_route_ordered` (`way_refs.rs:152`): the sequential residue there
is `route_way` per way (`sinks.rs:249`), which can be batched per chunk instead of called per way,
cutting mutex traffic by ~`ROUTE_CHUNK_WAYS`×.

Also expect a **peak-RSS win**: the `ClassifiedWay.topic_rows` transient is precisely what forced
`FOLD_CHUNK_BLOBS` down from 256 to 64 (`sorted.rs:125-133`). Removing it may allow raising the
chunk size again, compounding with Phase 1.2.

*Correctness gate:* row order must be byte-identical. Diff staged `.bin` files before/after on
brandenburg for every file backend — this is a pure refactor and any output diff is a bug.

*Exit gate:* re-run Phase 0, plus an ordered-vs-unordered A/B on `--output pg` via
`OSMNEXUS_FORCE_ORDERED` (the doc's own 236.8 / 371.8 protocol) to measure the remaining tax
directly and comparably.

### Phase 3 — Decision gate

**Resolved by §2b/§2c** — the gate has been run, without needing Phase 2.

What ordered tax is left: **+4-7% at the thread counts you would actually use**, of which the fold is
~3 s and `par_route_ordered`'s arena access is the rest. Is the join now the binding constraint:
**yes** — ~26 s, flat at every thread count, ~19% of the best run, larger than the entire ordered tax
at every point except t=56.

Original gate text below, for the record:

With Phases 1–2 measured, answer: **what ordered tax is left, and is the ~26 s join now the
binding constraint?**

- If the residual tax is small and the flat ~26 s join dominates, go to Phase 4 — a pure
  *throughput* change, with no join redesign at all.
- If a large ordered tax survives the fold fix, reconsider unordered routing on its own merits, and
  only then does sort/merge (Phase 4, "not doing") come back into scope.
- If both are small, **stop**.

This gate is the main structural difference from `output_plan.md`, which treats sort/merge as
already-decided.

### Phase 4 — Parallel single-file GeoParquet *(gated)*

Targets the flat ~26 s join/write that no thread count touches (§2.4). Output stays **one
`{table}.parquet` per topic** — a multi-file partitioned dataset would be trivially parallel and is
idiomatic GeoParquet, but it changes the output contract, so it is rejected.

Three sub-steps, each shippable on its own, in this order.

#### 4a — Stream the writer (no sharding, no parallelism)

`write_table_parquet` accumulates every merged row into Arrow builders, calls `builders.finish()`
to materialize one table-sized `RecordBatch`, then writes it in a single `ArrowWriter::write`.

Two things to be clear about. The **file is already correct** — `write` splits an oversized batch at
`max_row_group_size` internally, so the output is properly row-grouped; this is a *builder-side*
memory problem only, not a format or library limitation. And it is written that way for a **real
reason**: the `geo` key-value metadata is built from `geometry_types`, accumulated during the join
loop and passed to `writer_props()` at `ArrowWriter::try_new` — so the writer cannot be constructed
until the whole table has been scanned.

`ArrowWriter::append_key_value_metadata` dissolves that constraint (its own doc: *"a way to append
kv_metadata after write RecordBatch"*). New shape:

```
writer = ArrowWriter::try_new(file, schema, props_without_geo)
for each ~64k merged rows from the cursor join:
    writer.write(&batch)                    // flushes row groups as it goes
    geometry_types.insert(..)               // accumulated along the way
writer.append_key_value_metadata("geo", geo_json(geometry_types))
writer.close()                              // footer carries the geo metadata
```

Bounded memory — one row group plus one batch, instead of a whole table — with identical `geo`
metadata. Also removes a memory risk that exists *today*, independent of everything else here.

`write_geojson` has the same flaw and admits it (`geojson.rs:8`); the streaming half of this applies
there too, minus the footer-metadata problem. `write_geojsonseq` already streams correctly.

*Gate:* output must be **byte-identical**. Row order does not change at this step, so it is a pure
refactor. This is the last step at which a byte-identical test is possible.

#### 4b — Parallelize across topics

`write_parquet` is `for table in tables` — bikelanes and roads are written one after the other.
`par_iter` is free. Gains are capped by the largest table (roads dominates), so expect ~15-20% of
the join step, not half. Worth taking because it costs nothing.

#### 4c — Hash-partition + parallel column encode

- Partition tag and geometry rows on `h(osm_id) % S` — **not** round-robin (§1.4). Corresponding
  rows land in the same shard, relative order survives inside it, and `OrderedGeomCursor` works per
  shard unchanged. This is what makes `w > 1` safe.
- Join each shard on a rayon worker; encode its column chunks via `get_column_writers` /
  `compute_leaves` / `ArrowColumnWriter` — verified present in parquet 53.4.1.
- The main thread appends finished `ArrowColumnChunk`s to row groups **in deterministic shard
  order**, then writes the footer with the unioned `geometry_types`. Appending is a buffer copy;
  encoding and compression are what moved off the serial path.
- `--join-shards` for `S`, decoupled from `--threads`/`--db-writers`; default from Phase 0 data.

*Hard dependency:* `geojson` and `geojsonseq` read the same staged `{table}.bin` through the same
`cursor.rs`, so **4c cannot be parquet-only** — all three cursor-join consumers must be updated in
one change or the other two break. The per-shard join is identical for each, so the work is uniform.
See §5.

*Gate:* row order in the file becomes shard-grouped rather than blob-ordered, so the test is
**identical as a multiset of rows**, not byte-identical — the same standard used for the csv change
in Phase 1.1. Nothing downstream joins a Parquet file positionally (rows carry `osm_id`), so this is
an output change, not a correctness one. Confirm the live editor is unaffected.

#### Not doing: sort/merge + unordered-always

`output_plan.md`'s proposal exists to survive *unordered* routing, which destroys tag/geometry
correspondence. Hash-partitioning keeps that correspondence and still parallelizes — so the sort,
the k-way merge, and the `-1`-trailer shard framing are all unnecessary under this plan. Revisit
only if Phase 3 shows ordered routing must be abandoned on its own merits.

### Phase 5 — In-memory staging opt-in *(independent)*

Per `output_plan.md`, unchanged in substance, with one update: §2 measured ~8 GB peak on germany
against a 64 GB budget, so the memory headroom is larger than the doc assumed. Still opt-in —
`geojsonseq`'s streaming property is real and must not become collateral damage.

Can be built before or after Phase 4; it depends on neither.

### Phase 6 — `as_member` / retire the relation graph-fallback *(separate track)*

Unchanged from `output_plan.md`. Owns the accepted `public_transport` regression. Removes
`group_edges_by_way`, the one genuinely random-access join. Blocks nothing above and should not be
bundled into this work.

---

## 5. Cross-backend impact

This plan is overwhelmingly about the **file backends**. Stated plainly so nobody expects otherwise:

| | 1.2 `route_chunk_ways` | 1.3 `fold_chunk_blobs` | 2 fold | 4a stream | 4b par-tables | 4c hash-shard |
|---|:--:|:--:|:--:|:--:|:--:|:--:|
| `pg` | — | **yes** | — | — | — | — |
| `csv` | — | yes (now) | — | — | — | withdrawn |
| `geojson` | yes | after Phase 2 | yes | **yes** | yes | **must land with 4c** |
| `geojsonseq` | yes | after Phase 2 | yes | already streams | yes | **must land with 4c** |
| `parquet` | yes | after Phase 2 | yes | yes | yes | yes |

### `--output pg` is almost entirely untouched

- **1.2** — `materialize.rs:194-199` sends unordered runs down `par_route_all`, which has no chunking
  and is already fully parallel. pg never executes `route_chunk_ways`.
- **Phase 2** — pg is already unordered; it is the path Phase 2 is trying to catch up to.
- **Phase 4** — pg has no post-run join; it COPYs live.
- **1.3 is the exception, and the only pg win here.** `fold_chunk_blobs` is called from all three
  passes unconditionally, so pg pays the same fixed-barrier cost — and because its transient carries
  no row payload, raising it for unordered mode is risk-free. Shipped.

### `--output csv`

Phase 1.1 (already shipped) is the whole win: csv now takes the unordered fast path end to end.
Phases 4a/4b do not apply — csv has no join step, confirmed by measurement (`join_s = 0.0` vs
parquet's `0.2` on bremen). `w > 1` is withdrawn (Phase 1, item 4).

**Consequence worth knowing:** csv output is **no longer byte-reproducible between runs**. Unordered
routing fires from the parallel closure, so row order depends on rayon scheduling — verified by
running one binary twice on identical flags and getting differing bytes but identical row multisets.
Rows carry `osm_id`, so nothing that joins by id is affected, but any workflow that checksums or
diffs csv output needs to sort first.

### `--output geojson` / `geojsonseq` — the hard constraint on 4c

Both read the same staged `{table}.bin` through the same `cursor.rs`. **4c cannot be parquet-only:**
shard the staging layout and these two break unless updated in lockstep. The per-shard cursor join is
identical for all three, so the fix is uniform — but it has to land as one change.

Two extras that fall out:

- **`write_geojson` has the same 4a flaw**, and its own module doc says so (`geojson.rs:8`: the
  `FeatureCollection` form *"buffers the whole topic in memory and isn't"* streaming). The metadata
  half of 4a does not apply (no footer), but the streaming half does. `write_geojsonseq` already
  streams correctly.
- **4b applies to both** — `write_geojson` and `write_geojsonseq` are also `for table in tables`.

---

## 6. Risks

| Risk | Mitigation |
|---|---|
| Phase 2 silently reorders rows; cursor join produces mismatched tag/geometry pairs | Byte-diff staged `.bin` output before/after on brandenburg for every file backend. This is a refactor with an exact expected output. |
| Chunk-size changes regress peak RSS (the metric they were tuned for) | Phase 0 harness records peak RSS on every run; treat an RSS regression as a failing gate, not a footnote. |
| Benchmarks mislead due to machine load — this already happened once and produced a wrong conclusion in `output_plan.md` | Exclusive allocation, warm cache, ≥2 reps, commit hash recorded. |
| Phase 4c shards round-robin by mistake, silently mismatching tag and geometry rows | The partition function is the whole correctness argument: assert it is `h(osm_id) % S` for *both* tables, and test as a multiset of rows per shard, not just per file. |
| Phase 4c's row groups get appended in nondeterministic shard order, making output irreproducible | Append in fixed shard index order; encoding is parallel, appending is not. |
| Phase 4a changes `geo` metadata while moving it to `append_key_value_metadata` | Byte-identical gate at 4a catches any difference in the footer. |
| 4c ships for parquet alone, silently breaking `geojson`/`geojsonseq`, which share `cursor.rs` and the staging layout | Treat all three cursor-join consumers as one unit of work (§5). A parquet-only 4c is not a smaller change, it is a broken one. |
| Someone diffs or checksums `--output csv` output and finds it unstable after Phase 1.1 | Documented in §5: unordered output is byte-nondeterministic by design, multiset-stable. Sort before comparing. |
| Effort sunk into sort/merge that Phase 2 or hash-partitioning made unnecessary | That is exactly what the Phase 3 gate exists to prevent. |

## 7. Immediate next actions

1. ~~Phase 0 harness, Phase 1.1, 1.2, 1.3 (unordered half), re-baseline, ordered/unordered A/B~~ —
   **done**. Results in §2b/§2c.
2. ~~Phase 2~~ — **cancelled on measurement** (§2c). Worth ~3 s.
3. **Phase 4a — streaming writer.** Now the top item: the join is ~26 s and flat at every thread
   count, the largest single addressable cost left. Bounded-memory win on its own, byte-identical
   testable.
4. **New: make `par_route_ordered` traverse the arena sequentially.** §2c isolates +13.8 s at t=56
   from probing the MPHF arena in `kept_way_order` rather than scanning it in slot order — the only
   ordered cost that grows with thread count. In neither original plan. Resolve in slot order,
   buffer, emit in blob order.
5. **Phase 4c** — hash-partition + parallel encode, with `geojson`/`geojsonseq` in lockstep (§5).
6. **Phase 1.3, ordered half** — no longer blocked on Phase 2 (which is cancelled); needs its own
   look at whether the `topic_rows` transient can be shrunk independently.
