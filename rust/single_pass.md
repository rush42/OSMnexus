# Plan: streaming reader with a dense node store (cut peak RAM ~13 GB → a few GB)

## Context

Germany imports fine under osm2pgsql but the Rust pipeline "barely makes it" — it peaks around
~13–15 GB and swaps on a 16 GB box (which also explains the slow ~30 min processing). The cause
is architectural, not language: the current reader (`rust/src/osm/reader.rs`) **materializes
everything**, while osm2pgsql **streams**.

Current peak (Germany, estimates):
- `Vec<OsmWay>` — all 16 M ways held at once: each has a `Vec<(f64,f64)>` of coords + a
  `HashMap<String,String>` of tags → **~10–13 GB** (dominant).
- `node_coords: HashMap<i64,(f32,f32)>` (84 M) + `node_ids` map + a transient dup `Vec<i64>` of
  every way-ref → another **~3–5 GB**.

osm2pgsql avoids this by (a) a **dense node-location store** (the `--flat-nodes` mmap file, 8 B/node,
off-heap) and (b) **streaming ways**: resolve each way's geometry from the store, emit it, free it —
never holding all ways. This plan adopts both.

**Goal:** Germany runs in a few GB RSS (no swap), same outputs, ideally faster. Keeps the existing
rayon → mpsc → COPY processing pipeline; only the reader/feed changes.

## Design (osm2pgsql-style)

Two **parallel** passes (keeps `osmpbf`'s parallel block decode), but no way materialization:

1. **Pass 1 — fill a dense all-nodes location store.** Parallel `par_map_reduce` over the PBF;
   for every node write its coord into the store at its id. (Storing *all* nodes, not just
   referenced ones, is what lets pass 2 stream without first holding the ways to learn refs.)
2. **Pass 2 — stream highway ways.** Parallel `par_map_reduce`; for each `highway` way: look up its
   node coords in the store, build the `OsmWay`, run `process_way(&way, &runners)`, and send the
   resulting rows into the existing mpsc channel — then drop the way. The `map` closure returns
   `()`; reduce is a no-op. No `Vec<OsmWay>` ever exists.

**Node store** (`rust/src/osm/node_store.rs`, new):
- Primary: an **mmap'd flat file** indexed by node id — offset = `id * 8`, value = `(i32 lon_e7, i32 lat_e7)` (scaled 1e-7°, matching osm2pgsql; also fixes the current f32 precision loss at high latitude). Sparse file (`set_len` to `(max_id+1)*8`), so actual disk/page use ≈ 430 M × 8 B ≈ **3.4 GB**, off-heap and reclaimable by the OS → low RSS, and it scales to planet.
  - Parallel writes in pass 1 touch disjoint 8-byte slots, so they're data-race-free; expose the store as `&[AtomicI32]` over the mmap (or a documented `unsafe` slot write) to satisfy the aliasing rules.
  - `get(id) -> Option<(f64,f64)>` returns `None` for unwritten slots (sentinel, e.g. `i32::MIN`).
  - Need `max_id`: take it as a config cap (planet max ≈ 12e9 → ~96 GB *logical*, sparse) or a cheap first scan; start with a generous constant cap.
- Fallback (simpler first cut, if mmap is fiddly): in-RAM `Vec<(i64,i32,i32)>` filled in id order (PBF dense nodes are sorted) + binary-search lookup (~6–7 GB RAM). Lower risk, higher RAM; keep mmap as the real target.

**Dependency:** add `memmap2` to `Cargo.toml` for the flat-file store.

## Code changes

- **`rust/src/osm/reader.rs`** — replace `read_highway_ways(path) -> Vec<OsmWay>` with:
  - `build_node_store(path, &cfg) -> NodeStore` (pass 1), and
  - `stream_highway_ways(path, &store, |way| …)` **or** expose the pass-2 `par_map_reduce` directly
    so `main` can run `process_way` + `tx.blocking_send` inside the map closure.
  - Drop `WayData`, the `node_ids`/dup-`Vec<i64>` machinery, and `resolve_way`'s `Vec` collection.
- **`rust/src/osm/node_store.rs`** (new) — the mmap flat store described above.
- **`rust/src/main.rs`** — restructure the producer: instead of `read_highway_ways → ways` then
  `ways.par_iter().for_each(process)`, do pass 1 (build store) → log → spawn the blocking pass-2
  task that streams ways through `process_way` into the channel. The COPY-sink/consumer half is
  unchanged. Geometry/length/meta still computed per way in `process_way`.
- **`rust/src/osm/types.rs`** — `OsmWay` can keep its shape; it's now short-lived (built, processed,
  dropped). Optionally store coords as already-projected to cut transient work (not required).

What stays put: the rayon+`mpsc`(512)+per-topic COPY pipeline, the deadpool `Object`-retention fix,
and all topic/engine logic.

## Verification

1. `cargo build --release`.
2. Berlin parity (separate `osm` DB): `PGDATABASE=osm PGUSER=rush42 ./target/release/osm-pipeline /tmp/berlin-small.osm.pbf` → counts unchanged (`bikelanes 1652, roads 11630, barrierLines 0`).
3. **Germany memory + correctness** (the point): run on `germany-latest.osm.pbf` into a fresh
   `osm_germany` while watching `ps -o rss -C osm-pipeline` (or `/usr/bin/time -v`):
   - peak RSS should be a few GB (≈ node store + small in-flight), not ~13 GB; no swapping.
   - row counts match the last good run: `bikelanes 1005569, roads 13116084, barrierLines 170293`.
   - wall-clock should drop (no swap thrash; single materialization gone).
4. Spot-check a few way geometries vs the previous run (the i32-e7 store should match or slightly
   improve coordinate precision vs the old f32 store).

## Notes / follow-ups (out of scope here)

- **Single pass to save the second file read.** Both recommended passes are already parallel
  (`par_map_reduce`), so this is purely about avoiding the second read (~7–10 min), not about
  parallelism. A *naive* single pass would have to read blobs in file order (sequential
  `for_each`) to guarantee nodes-before-ways — that's the only thing that would drop parallel
  decode. To keep parallel decode in one pass you build an ordered blob pipeline by hand (drop to
  `osmpbf`'s `BlobReader`: sequential blob I/O, fan decode out to a thread pool, consume decoded
  blocks in file order via a bounded reorder window so the node store is complete before way-blocks
  are processed). Worth it only if read time, not memory, becomes the limit.
- **node→way index for the graph**: pass 2 is the natural place to also accumulate node→[way]
  adjacency for the "informative edges" graph goal — deliberately not included here to keep this
  change to the memory/streaming fix.
- **Parse cache**: serializing the resolved ways/store to disk to skip re-reads is a separate QoL
  idea, not part of this.
