# Output module redesign — status and plan

Working notes from an extended design discussion on the `output` module: what's shipped, what's
measured, and what's proposed but not yet built. Read `BACKLOG.md` for the rest of the project's
deferred work; this file is scoped to the output/staging/join redesign specifically.

## Done and committed (on `main`)

### One row encoding, pluggable sinks

Every output row type (`TopicRow`/`MemberRow`/`EdgeRow`/`GeomRow`/`NodeRow`) used to hand-maintain
two near-duplicate serializations: `CsvRow` (text) and `BinaryRow` (Postgres `COPY FORMAT BINARY`,
used only by `--output pg`). `CsvRow` is retired. `BinaryField` — the existing per-column enum
`BinaryRow` already produced — is now the *one* canonical encoding every sink starts from:

- `copy_writer` streams it live to Postgres (`--output pg`, unchanged).
- `csv_writer` (`--output csv`) now calls `binary_fields_to_csv_row` — one shared, type-agnostic
  function that stringifies `BinaryField` — instead of a hand-written `csv_fields` per row type.
- `binary_file_writer` (new) writes the same wire bytes to a `.bin` staging file instead of a live
  Postgres connection — used by `geojson`/`geojsonseq`/`parquet` staging (was `.csv` staging before).
- `memory_sink` (new, not yet wired to a caller) forwards row batches to an in-process channel with
  no encoding at all — infrastructure for embedding the pipeline as a library later.

Reading the `.bin` staging files back needed a decoder that never existed before (only Postgres
itself ever consumed this wire format): `read_binary_header`/`read_binary_row`/`FromBinaryRow` in
`src/output/rows.rs`, generic over `std::io::Read` so the same decode logic works streamed off disk
(`BufReader<File>`, needed since a tag table can be gigabytes) or from a small in-memory read
(`edges.bin`/`relation_members.bin`).

### Shared forward-cursor join (`src/output/cursor.rs`)

`output::geojson` and `output::parquet` both need to join a topic's tag rows to its geometry rows
after the run. This works without a hashmap for node/way/relation geometry and a way's own graph
edges, because `geom::materialize` already resolves+routes geometry in the same order the select
phase routed the matching tag row (`SelectionContext::kept_way_order`/`kept_relation_order`) — so a
geometry file's `osm_id` sequence is a subset of the tag file's same-kind sequence, in matching
relative order. `OrderedGeomCursor` and `EdgeCursor::get_all` walk both forward in lockstep instead
of building an in-memory map. The one case that genuinely needs random access — a relation's
graph-fallback geometry, whose member way ids are scattered non-adjacently across the whole way
region — still uses a hashmap (`group_edges_by_way`), unavoidably.

This machinery used to be duplicated per consumer; it's now shared in `cursor.rs` so `geojson.rs` and
`parquet.rs` both build on the same cursors, WKB encode/decode, and edge/relation-member readers.

### GeoParquet output ported onto this infrastructure

The `geoparquet-rebase` branch (Arrow-staged, its own separate CSV/Arrow join machinery) and the
original `simon/feat/geoparquet-output` branch (older schema, CSV-staged) are superseded. Merged
current `main` into a new branch off the *original* (unrewritten) branch tip via a real merge commit
— not a rebase, so pushing was a fast-forward, no force needed — then rewrote `output::parquet` to
stage through the same `.bin` files and `cursor.rs` machinery as `geojson`, rather than its own Arrow
staging. Pushed to `simon/feat/geoparquet-output`.

Fixed a real compatibility bug along the way: arrow-rs 53.4.1's default `EnabledStatistics::Page`
writes page-index size-statistics that pyarrow 19 can't read back (`"Repetition level histogram size
mismatch"`); switched to `EnabledStatistics::None`.

### Measured: binary staging vs CSV staging (brandenburg-latest.osm.pbf)

| | CSV | binary (`.bin`) | delta |
|---|---|---|---|
| tag tables (combined) | 296.53 MB | 300.36 MB | **+1.3%** |
| geometry tables (combined) | 464.94 MB | 275.70 MB | **-40.7%** |
| **total** | 761.47 MB | 576.05 MB | **-24.3%** |

Tag tables are slightly *larger* in binary (per-field length-prefix framing costs more than CSV's
1-byte comma, and tag rows are mostly JSON text so there's little numeric-encoding win to offset
that). Geometry tables are ~41% smaller because CSV had to hex-encode WKB (exactly 2x blowup);
binary stores it raw. Net win because geometry dominates total size here.

## Verified, corrected a wrong earlier claim: `ordered` has a real cost

The pipeline has an `ordered` flag (`osm::reader::Callbacks::ordered`) — `true` for `csv`/`geojson`/
`geojsonseq`/`parquet` (need tag/geometry row order to match for the cursor join above), `false` for
`pg` (correlates by `osm_id` column, no order needed). An initial claim mid-session that "ordered and
unordered cost about the same after a batching fix" was **wrong** — it was measured without checking
system load, on a machine that turned out to be under varying background load across runs.

Re-measured cleanly (`--output pg`, `ordered` force-flagged via a temporary
`OSMNEXUS_FORCE_ORDERED` env var in `main.rs`, `uptime` checked quiet before each run,
germany-latest.osm.pbf, `--threads 8 --db-writers 8`):

| mode | wall time | Pass A | Materialize |
|---|---|---|---|
| unordered (default for `pg`) | **236.8s** | 109.7s | 40.4s |
| ordered (forced) | **371.8s** | 197.7s | 92.8s |

**Ordered is 57% slower.** Both Pass A and materialize roughly double. The per-blob tag-routing
batching fix from earlier this session reduced *mutex contention* on `route_tag` calls but did not
remove the underlying serialization: ordered mode still funnels every blob's accumulator work
(`way_refs`/`counts`/`kept_way_order` in Pass A; `par_route_ordered`'s chunked resolve-then-
sequential-route in materialize) through one thread per `FOLD_CHUNK_BLOBS` chunk. This is the
`ordered`-mode tax `csv`/`geojson`/`geojsonseq`/`parquet` currently all pay.

(The `OSMNEXUS_FORCE_ORDERED` env var is still in `main.rs` — harmless, off by default, useful for
re-verifying this in the future.)

## Proposed, not yet built: unordered-always + sort/merge join

Given the ordered tax is real and large, proposal: drop the `ordered`/`unordered` split entirely —
always route unordered (full parallelism, no fold bottleneck, matching `pg`'s existing fast path) —
and replace the position-matched cursor join with a sort + k-way merge join for `geojson`/
`geojsonseq`/`parquet`.

### Why not just use a hashmap join instead (simpler)?

Considered and rejected as the general strategy. A hashmap join is O(n) average (cheaper in raw
operation count than sort+merge's O(n log n)) but requires the *entire* geometry table resident in
one random-access structure for the whole join — at germany scale that's gigabytes, and once a table
exceeds cache size, hashmap lookups are effectively random memory access (cache-miss-per-lookup).
This is exactly what the forward-cursor join design (see above) already deliberately avoids for the
node/way/relation/own-edges case — a general hashmap join would be a regression back to that. Sort +
merge trades worse Big-O for bounded memory and sequential (cache-friendly) access throughout, which
matters more at this scale than operation count does. (The one remaining hashmap,
`group_edges_by_way` for relation-fallback lookups, is kept as small as possible rather than being
the default strategy — it's unavoidable only because that access pattern is genuinely random.)

### Shard sizing

A shard's size is `table_rows / S` where `S` is the shard/partition count — not the whole table. At
germany scale, `bike.bin` is roughly 2GB (extrapolated from the brandenburg byte/row ratio); with
`S = 8` that's ~250MB/shard, comfortably sortable in memory (the pipeline already holds larger
structures resident at once, e.g. `node_coords` at ~1.2GB on the same run). So per-shard in-memory
sort is not a large engineering lift.

### Shard/partition count is independent of thread count

`S` (partition count) only decides which round-robin bucket a row lands in — it's not tied to how
many threads actually process shards. A pool of `--threads` workers can process `S ≫ threads`
partitions via work-stealing, same as rayon already does for blob decoding. For file outputs there's
also no Postgres-connection reason to tie `S` to `--db-writers` (that only matters for `--output pg`
at all). Real tradeoff in picking `S`:

- **More shards**: cheaper individual sorts, and less *total* sort work (`S` sorts of `n/S` beats
  fewer bigger sorts asymptotically) — but a wider final merge (`O(n log S)`, grows with `S`) and
  more per-shard bookkeeping/boundaries.
- **Fewer shards**: cheaper merge, bigger individual sorts.

Proposal: a separate `--join-shards` flag, decoupled from `--threads`/`--db-writers`, defaulting to
something like `--threads` or a fixed value (16-32) pending actual tuning.

### Shard boundaries without a manifest or multi-file naming

Don't write `{table}.0.bin`, `{table}.1.bin`, ... and require the join step to discover/enumerate
them. Reuse the wire format's existing trailer sentinel instead: a field count of `-1`
(`write_binary_trailer`) already means "end of data" and `read_binary_row` already returns `None` on
it. Each shard's sorted run ends with this same trailer; the reader just needs to know `S` (already
known — it's the flag above) to count how many `-1` markers mean "next run" versus "true EOF" (the
`S`-th one). Shards can still be written to their own temp files independently (no concurrent-write
coordination needed) and concatenated into one file afterward — cheap `io::copy`-level byte
concatenation, stripping the repeated 19-byte header from every shard after the first.

### Scope of this change

Only affects `geojson`/`geojsonseq`/`parquet` (the three backends that currently set `ordered =
true`). `--output pg` is already unordered and unaffected. `--output csv` doesn't need ordering at
all today (no cross-file join happens for a plain CSV dump) — worth confirming it's already on the
cheap path, or fixing it to be, independent of this work.

**Not yet decided against**: parallelizing the *ordered* fold itself (e.g. concatenating per-blob
results in already-known order instead of accumulating through a sequential fold, mirroring the
`way_refs` Vec-instead-of-HashMap trick from earlier this session, where order bookkeeping turned out
to need no real cross-blob computation) was raised as an alternative to abandoning `ordered` entirely.
Not scoped out in detail — the sort/merge direction was explored further because the ordered tax
turned out to be large enough to make "stop paying it at all" attractive, but a from-first look at
whether the fold specifically can be de-serialized wasn't done.

## Proposed, not yet built: in-memory staging opt-in (no disk at all)

Separate idea, raised before the sort/merge discussion: for `geojson`/`geojsonseq`/`parquet`, skip
disk staging entirely as an opt-in mode.

Tag rows are fully known by the end of the select phase (routed in `kept_way_order`/
`kept_relation_order` order). If they're held in memory (`Vec<Vec<TopicRow>>`, indexed by position)
instead of written to `.bin`, the materialize phase can join **inline**: as each way/relation's
geometry resolves, look up its already-known tag row(s) by position (same order, so it's a direct
indexed lookup, not a cursor) and emit the finished output row (GeoJSON `Feature` / Parquet row)
immediately. Geometry never touches disk at all in this mode — no `way_geom.bin`/`node_geom.bin`/
`relation_geom.bin`/`edges.bin`, and no `OrderedGeomCursor`/`EdgeCursor` read-back pass, since those
exist specifically to reconcile two on-disk passes that this mode wouldn't have.

`relation_members` (for the graph-fallback case) can equally be built and kept as an in-memory
hashmap from the select phase instead of written to `relation_members.bin` — no loss there either.

**Why opt-in, not default**: this trades away the current bounded-memory guarantee. Nothing today
holds more than one flush-batch of rows at a time (deliberate — module doc notes an earlier version
of the pipeline OOM'd on a country-sized import by buffering tag rows). Holding a whole table's tag
rows in memory is fine at brandenburg scale (~300MB) but risky at germany-or-bigger scale. `geojson`'s
own doc already frames the format as "for local tooling like the live editor," so this is a reasonable
trade for that use case specifically — but `geojsonseq` currently *streams* to disk without buffering
a whole topic, so it's the one output that would lose a real property it has today if this became the
default rather than opt-in.

**Relationship to the sort/merge proposal above**: independent. In-memory staging is about whether
tag/geometry rows touch disk at all; sort/merge is about how the join works when they do. Both could
coexist (in-memory mode wouldn't need either the `ordered` fold *or* sort/merge, since holding tags
indexed in memory sidesteps the whole "how do these two files line up" problem) — but doing
in-memory staging well doesn't require solving the file-based join problem first, and vice versa.

## Proposed, not yet built: retire the relation→edge graph-fallback join via `as_member`

Reconsidering the relation graph-fallback case (see the earlier "where does the zip join not work"
discussion) rather than just working around it. The underlying issue: edges *are* ways (one way
decomposed into several intersection-cut segments) — the current design instead treats a relation
wanting graph-shaped output as the primary output row, reaching out at output-build time to *borrow*
geometry from its member ways' edges (`push_graph_fallback_features` in `output/geojson.rs`/
`output/parquet.rs`: the relation's own tags, paired with each member way's edge segments). That's
what forces `group_edges_by_way`'s hashmap — a relation's member way ids are an arbitrary, unordered
set scattered across a completely different pass's output (`edges.bin`, ordered by `kept_way_order`,
not `kept_relation_order`), so no forward cursor can serve the lookup (see the fuller explanation
above).

**Proposed fix**: flip which element owns the output row. Instead of a relation borrowing edge
geometry at output time, an edge/way should carry whatever relation-derived context it needs *as part
of its own tag row*, computed during the way's own classification (Pass A) — the same `as_member`
mechanism already sketched for pulling parent-relation tags into a way/node as a small derived
annotation set (see `feedback_as_member_relation_annotations` in project memory: a reverse index
way/node → relation ids, paired with a compact per-relation annotation store, *not* full tag maps
kept resident). Under this model, an edge already knows everything about itself — including which
relations reference it — by the time it's classified, before geometry or output enters the picture at
all. This removes the relation-fallback branch, `group_edges_by_way`, and the one hashmap-requiring
join entirely; every remaining geometry join becomes the clean "this element's own geometry" case (see
the `ordered`-tax discussion above — this doesn't eliminate that tax, since way/node/relation-*own*
geometry still needs order-matching, but it does remove the one case that couldn't be solved by fixing
`ordered` alone).

**Until `as_member` ships for this**: `--config public_transport` is the one shipped config that
actually exercises the relation-fallback path today (per `BACKLOG.md`'s cursor-join history) — its
route relations currently produce Features by borrowing member-way edge geometry. Removing the
fallback without replacing it is an accepted, flagged regression for that config specifically: its
route relations would produce no graph-fallback output at all until `as_member`-derived edge
annotations land. Not yet removed from the codebase — this is the plan for when it is.

## Open decisions before implementing

1. Sort/merge join vs. parallelizing the ordered fold — not resolved; sort/merge was explored further
   but the fold-parallelization alternative was never actually scoped out to compare against.
2. `--join-shards` default value — needs real tuning data, not just the "smaller sort, wider merge"
   tradeoff description above.
3. Whether in-memory staging (separate proposal) supersedes or complements the sort/merge work for
   `geojson`/`parquet` specifically, given it would remove the need for either ordered-fold-parallelism
   or sort/merge for those two backends' join — `geojsonseq` still needs *something* for its streaming
   case regardless.
4. `--output csv`'s own `ordered = true` setting — confirm it doesn't need to be true (no join happens
   for plain CSV output) and fix if so; unrelated to the bigger redesign but low-cost to check.
5. `as_member`-for-edges design/implementation itself isn't scoped yet — this file only records the
   decision to go that direction and the accepted `public_transport` regression until it's built.
