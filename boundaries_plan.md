# Module boundaries: plan for the lang / codegen / pipeline split

Working plan for making the existing responsibility boundaries explicit and enforcing them as
crates. Based on `develop` @ `e2fea85`. Every step is a behaviour-preserving refactor and its own
PR into `develop`; none changes pipeline output. See `BACKLOG.md` for the summary entry.

## Status (branch `refactor/boundaries`)

| Step | State | Notes |
|---|---|---|
| 0 | done | `scripts/regress.sh`; baseline from `develop` @ `e2fea85` |
| 1–4 | done | `lang::expr`, `lang::tags`, `lang::json`; stale crate deleted |
| 5 | done (hot path) | `Filter::InSet` holds a resolved `ValueSetRef`; `DirectedExtract` precomputes its keys. Deviation: the set is a `&'static` borrow of the existing table rather than a `SetId`, because the table source is still the global (step 6). Borrowing never touches a refcount, so the contention concern doesn't apply; revisit the representation together with `CompileEnv`. Not done: the `length` unit table is still looked up per `parse_length` call (needs manual `Ord`/`Hash` on `Builtin`; small win, deferred). |
| 6 | done | `ConfigSources` (config dir as data) + `CompileEnv` (value sets, unit tables, traffic side); `Topic::load(src, env, name, depth)` and the overlap linter are pure; the shell's `config_dir::Config` is the only code that reads the config directory. Removed: `paths::CONFIG_ROOT`/`set_config_root`, the `value_sets`/`units` `OnceLock`s, and the `traffic` module. `parse_length` now carries its unit table, `InSet` an `Arc` set. Deviation: the env reaches the hand-written `Deserialize` impls through a scoped thread-local (`lang::env::with_env`), because serde derive can't pass a context; it's per call and per thread, never process-global. |
| 7 | done | `Topic` (`topic::model`, built once at startup) + the free function `topic::pipeline::process` for per-element evaluation (an interim `CompiledTopic` + `TopicRunner` newtype was dropped again: the wrapper held no state, and `Deref`-as-inheritance hid that); `OutputSchema`; `Send + Sync` guard. Open question found: the empty-tags probe (`skip_untagged`) runs the real pipeline (`build_topic_rows`) at compile time, so `topic::model` depends on `topic::pipeline` |
| 8 | half | boundary check in CI (`scripts/check_boundaries.sh`); workspace split not started |
| 9–11 | open | |

Every done step: `scripts/regress.sh` identical on `tiny.osm.pbf` + Berlin (incl. all
`tree_json`/`export_ts_types` output), 86 lib tests green. Release build on Berlin (2-core
machine, interleaved runs): 1 thread 14.7 s vs 14.75 s, 2 threads 9.09 s vs 9.17 s (7 runs each,
within noise), peak RSS unchanged (398 MB / ~410 MB). Note: `--threads` defaults to 1, so
benchmarks must pass it explicitly. A full `bench_sweep.sh` + `dhat` run on a bigger machine is
still due before the workspace split.

## Principles this plan follows

Each one is here because it fixes something concrete in this codebase, not for its own sake.

1. **Dependency rule.** Dependencies point inward, towards the language: pipeline → lang,
   codegen → lang, never back. Enforced by Cargo once split, by a CI check before that.
2. **Functional core, imperative shell.** `lang` is pure: data in, compiled topic out. No
   filesystem access, no process-global state. All I/O (reading config dirs, PBF, Postgres,
   writing outputs) lives in the shell. Today the frontend reads files in `topic::load`,
   `categorize::{linter, transform}`, `value_sets`, `units`, and `decision_tree/optimal.rs`.
3. **Explicit context over globals.** `paths::CONFIG_ROOT` (`OnceLock`), the lazily file-loaded
   `value_sets`/`units` tables, and `traffic::LEFT_HAND_TRAFFIC` (`AtomicBool`) are hidden inputs
   to compilation. They make two configs in one process impossible (in-process editor, WASM,
   parallel tests) and make a function's real inputs invisible. They become fields of an explicit
   `CompileEnv` passed in.
4. **Parse, don't validate; one type per phase.** The compiler phases get distinct types, so a
   later phase can't receive input an earlier phase hasn't processed: `RawDecl` (serde shapes +
   sugar) → `ResolvedDecl` (macros, sanitizer refs, shared producers inlined) → `Topic`
   (decision tree built, output schema inferred). Today ref inlining runs on `serde_json::Value`
   *before* deserialization, so unresolved refs are untyped.
5. **The output schema is a first-class compile result.** Type inference for output fields
   currently lives inside `bin/export_ts_types`. It moves into `lang` as `OutputSchema`; every
   backend (TS, TileJSON, JSON Schema, docs) consumes that one value, so they can't disagree.
6. **Ports and adapters, but only where there are real alternatives.** Sources (`pbf`, `csv`,
   `postgis`) and sinks (`pg`, `csv`, `geojson[seq]`, `parquet`, `memory`) already are adapters;
   formalise them as traits in the pipeline and gate heavy adapters behind Cargo features
   (`postgres`, `parquet`) rather than separate crates.
7. **Small public surface.** Each crate exposes a deliberate facade (`compile`, `Topic`,
   `OutputSchema`, `eval`, diagnostics); internals are `pub(crate)`. That's what makes later
   refactors inside a crate free.
8. **Contracts are versioned and tested.** The editor talks to `osmnexus`/`tree_json` over CLI +
   JSON. That JSON is an API: give it a version field, generate its TS types from Rust, and
   snapshot-test it.
9. **Diagnostics are data.** Compile errors carry file, JSON path (and position where possible),
   so the CLI, CI and editor can all render them. Required for a DSL others will write.
10. **Fitness functions and recorded decisions.** Boundaries are checked in CI, not by memory;
    decisions that took effort to reach (like this one) get a short ADR under `docs/adr/`.
11. **Move work from run time to compile time.** Anything that depends only on the config (name
    lookups, table resolution, normalisation of literals) is done once in `compile()`, so the
    per-element hot path does strictly less work. The compiled IR holds resolved data, not names.
    This is also why removing the globals is not a performance cost: `value_sets.json` (829 B)
    and `units.json` (153 B) are trivially cheap to load eagerly, and the lazy `OnceLock`s only
    provided `&'static` convenience.
12. **No unmeasured performance changes.** Every PR from step 5 on is compared against a
    release-build baseline (see step 0). Refactors must be neutral or faster.
13. **The compiled IR is immutable, `Send + Sync`, and shared by reference; parallelism belongs
    to the shell.** Today `&[Topic]` (then `&[TopicRunner]`) is shared read-only across rayon workers
    (`osm::reader::fallback`'s `par_map_reduce`, `way_refs::par_route_*`, `processing::classify_*`).
    That must stay exactly so: no interior mutability, no per-thread clones of the IR, no
    reference-count traffic in the hot path. `lang` itself stays single-threaded, pure code
    (no rayon, no tokio); the shell decides how to parallelise.
14. **No new per-element allocations.** The hot path today borrows tags as `Cow<'a, str>` straight
    from the PBF blocks. New APIs must keep borrowing (return `Cow`/`&str`, fill caller-owned
    buffers), and must not turn borrowed data into owned `String`s at a boundary.

**Deliberately not doing:** more than three crates (adapters stay features), a plugin/DI
framework, async in `lang`, or a generic "IR for any OSM tool". Revisit only when a second real
consumer needs it.

## Target shape

- **`osmnexus-lang`: the functional core.**
  - `lang/*` (+ new `lang::expr`, `lang::tags`, `lang::json` from `parser.rs`), `topic::spec`, the
    pure part of `topic::load`, `categorize::{categories, transform, linter}` (linting logic only),
    `decision_tree/*`, and the *data types* of `value_sets`/`units`/`traffic`.
  - Public API (sketch):
    ```rust
    pub struct CompileEnv { value_sets, unit_tables, left_hand_traffic, tree_max_depth, … }
    pub struct ConfigSources { files: BTreeMap<RelPath, String> } // contents, not paths
    pub fn compile(env: &CompileEnv, src: &ConfigSources, topic: &str)
        -> Result<Topic, Diagnostics>;
    impl Topic { pub fn schema(&self) -> &OutputSchema; pub fn eval(&self, tags: &RawTags, …) }
    ```
  - Dependencies: serde, serde_json, rustc-hash, thiserror. Nothing async, no I/O crates.
- **`osmnexus-codegen`: views of the compiled IR.** `tree.rs`, the bodies of
  `bin/{export_ts_types, plot_tree, tree_json}`, later TileJSON / JSON Schema / Mermaid / JGF.
  Input: `&Topic` / `&OutputSchema` only.
- **`osmnexus` (pipeline): the imperative shell.** Config-dir loader (filesystem → `ConfigSources`
  + `CompileEnv`), `osm::reader`, `geom`, `output`, `db`, `processing`, `csv_source`, `config`,
  `main`, `topic::{runner, pipeline, inherit}`, `bin/check_overlaps`. Source/sink adapters
  behind features.

## Current leaks (develop @ `e2fea85`)

| Leak | Where | Principle | Step |
|---|---|---|---|
| Stale crate that can't build | `crates/topic-engine-core` `#[path]`s into nonexistent `src/{classify,engine,lint,transform}` | — | 1 |
| Expression IR inside the linter | `decision_tree/*` (6 files) and `topic::runner` import `Expr`/`Literal`/`Predicate`/`NumOp`/`filter_to_expr`/`to_nnf` from `categorize::linter` | 1 | 2 |
| Core reads a CLI constant | `decision_tree/optimal.rs` → `config::DEFAULT_TREE_MAX_DEPTH` | 1 | 3 |
| Tag types in the reader module | `lang`, `topic::spec`, `categorize::*` → `osm::types::{RawTags, ElementKind}` | 1 | 4 |
| `lang` ↔ `parser` cycle | `parser` deserializes `lang` types; `lang` calls `parser::{TagSet, parent_or_obj, deserialize_sanitize_chain}` | 1 | 4 |
| Process-global compile inputs | `paths::CONFIG_ROOT`, `value_sets`/`units` `OnceLock` file loads, `traffic::LEFT_HAND_TRAFFIC` | 2, 3 | 5 |
| File I/O inside the core | `topic::load`, `categorize::{linter, transform}`, `value_sets`, `units`, `decision_tree/optimal.rs` | 2 | 6 |
| Compiling needs the runtime type | `export_ts_types`, `plot_tree`, `tree_json` call `TopicRunner::load` but only read compiled fields | 1, 5 | 7 |
| Type inference lives in a bin | `bin/export_ts_types` (`Contribution::{Closed, Open}`) | 5 | 7 |
| Untyped ref inlining | `topic::load::inline_{macro,sanitize}_refs` rewrite `serde_json::Value` before deserialization | 4 | 10 |

Allowed (shell → core): `geom::plan`, `geom::materialize`, `output::sinks`,
`topic::{runner, pipeline}` depending on `topic::spec`/`lang`/`categorize`.
Intra-core and fine: `lang` ↔ `decision_tree` (`lang/producer.rs` embeds a `DecisionTree`); keep
them in one crate.

## Steps

Ordered so each PR is small, mechanical where possible, and leaves `develop` releasable
(incremental/strangler style: the old paths keep working until the last consumer moves).

**0. Safety net.** `scripts/regress.sh`: for `fixtures/tiny.osm.pbf` and a Berlin extract, run
`--output geojsonseq` and hash each topic's *sorted* features; capture stdout of `export_ts_types`
and `tree_json` for every topic in `configs/tilda`. Every later step must reproduce these
byte-for-byte. Berlin output (dev-fast, 2 cores): ~113 s, ~1.4 GB peak RSS, 43,705 `bikelanes`
/ 352,521 `roads` features. **Performance baseline,** recorded in the PR:
- `scripts/bench_sweep.sh` (the existing `{backend} × {threads}` harness: per-phase timings and
  peak RSS, repeated runs) on a release build. The thread dimension is what shows whether a change
  hurts scaling, not just single-thread speed.
- A `--features dhat` run: total allocation count and bytes. Allocations *per element* are the
  most sensitive regression signal for steps 4, 5 and 7, more so than wall time.
- `benches/topic_pipeline.rs` (criterion) for the classify hot path in isolation.
Berlin fits on a small machine; the Germany numbers in `BACKLOG.md` (12.2 GB peak) stay the
reference for the full-scale check before step 8 lands.
Also add the first ADR (this plan's target shape).

**1. Delete `crates/topic-engine-core`.**

**2. Extract `lang::expr`** (`Expr`, `Literal`, `Predicate`, `NumOp`, `filter_to_expr`, `to_nnf`,
`prefix_expr_tags`) from `categorize::linter`; temporary re-export, update imports, drop
re-export.

**3. Invert the config constant.** `DEFAULT_TREE_MAX_DEPTH` lives in `decision_tree`; `config.rs`
reads it (move its benchmark-history comment with it).

**4. Tag types and JSON sugar into `lang`.** `RawTags`/`ElementKind` → `lang::tags` (re-exported
from `osm::types`); `parser.rs` → `lang::json`. Removes the cycle. Move the alias **unchanged**:
`RawTags<'a>` stays `FxHashMap<Cow<'a, str>, Cow<'a, str>>`, borrowing from the PBF block. Turning
it into an owned map at the crate boundary would add allocations per tag per element.

**5. Introduce `CompileEnv`; resolve config references at compile time.** Value sets, unit
tables, traffic side and tree depth become fields loaded once by the shell. `CompileEnv` is only
an input to compilation; it is **not** threaded through eval. Instead, compilation resolves every
reference into the IR:
- `Filter::InSet { in_set: String }` → a `SetId(u32)` index into a set table owned by
  `Topic`, borrowed during eval. Today `filter.rs:151` does an `OnceLock` check plus a
  by-name hash lookup on *every* evaluation; afterwards it's one `contains`. Prefer the index over
  storing an `Arc` per filter: each set is stored once however many filters (and decision-tree
  copies of them) reference it, and eval never touches a reference count. Cloning an `Arc` per
  evaluation from many rayon threads would make the count's cache line bounce between cores.
- The `length` unit table used by `sanitize.rs:289` is resolved once into the sanitizer, instead
  of `unit_table("length")` per call.
- The traffic side is baked into the `directed` extract / transform step at compile time instead
  of a global atomic read per element.
- Unknown set or table names become compile errors (`Diagnostics`) instead of runtime panics.
Keep the old global accessors as shims for code not yet migrated, and delete them in step 8.
This is the largest mechanical diff, and the first one that should measurably *speed up* eval.

**6. Move file I/O to the shell.** Introduce `ConfigSources` (relative path → contents). The shell's
loader walks the config dir once and fills it; core functions take `&ConfigSources` instead of
paths. `categorize::linter`'s directory walking moves to the shell; its checks stay in the core.
`ConfigSources` lives only for the duration of `compile()` and is dropped afterwards;
`Topic` must not keep the raw JSON text or `serde_json::Value` trees alive.

**7. `compile()` and `OutputSchema`.** Split the old `TopicRunner::load` into the pure
`lang::compile(env, sources, topic) -> Topic` and per-element evaluation as a free function
(`topic::pipeline::process`) over a
`Topic`. Move the closed/open inference from `export_ts_types` into
`Topic::schema()`. Switch the three codegen bins to `compile`. Keep `tree_json`'s CLI and
JSON unchanged. This is the step that needs the most careful review: the merge order of shared vs
topic-local macros/sanitizers must be preserved exactly; the step-0 snapshots catch deviations.
Topics are held by value in a `Vec<Topic>` (or `&'static` after leaking once at startup), not
behind an `Arc` that eval would clone. Add a compile-time guard so the IR can't silently lose
thread-safety: `const _: () = { fn assert<T: Send + Sync>() {} let _ = assert::<Topic>; };`
Since `load_all` compiles independent topics, the shell may compile them in parallel at startup;
optional, startup is not the bottleneck.

**8. Enforce the boundary in CI, then split the workspace.** First a CI check over `use crate::`
lines that fails if a core module imports `osm::reader`, `geom`, `output`, `db`, `processing`,
`csv_source`, `config`, `topic::{runner, pipeline, inherit}`, or uses `std::fs`/`OnceLock`. Once
green, create the workspace with `crates/osmnexus-lang` (minimal deps, `#![forbid(unsafe_code)]`)
and keep facade re-exports in the root crate for one release so the editor and paths keep
working. Remove the global shims from step 5. **Check cross-crate inlining:** hot small
functions (`Extract::read_str`, `filter::eval`, sanitizer steps) now sit in another crate. The
release profile (`lto = "thin"`, `codegen-units = 1`) should keep them inlined; confirm against
the step-0 baseline and add `#[inline]` where a regression shows up.

**9. `crates/osmnexus-codegen`** with a `Backend` trait over `&Topic`; bins become thin
CLIs with unchanged names and output. `insta` snapshots per backend (seeded from step 0).
Version the `tree_json` output and generate its TS types (`ts-rs`/`specta`) for `DagView`.

**10. Phase-typed resolution (optional, after the split).** Replace the `serde_json::Value`
rewrites with `RawDecl → ResolvedDecl` on typed data, and attach JSON paths to `Diagnostics`.
Only worth it once the DSL has more authors than you; until then the step-0 snapshots and the
linter cover the risk.

**11. Pipeline ports.** Formalise `Source`/`Sink` traits over what `main.rs`/`output::sinks` already
dispatch on, and move `postgres`/`parquet` adapters behind Cargo features (parquet already is).
Use static dispatch (generics or an enum over the adapters), or dispatch per batch as
`memory_sink` already does; never a `dyn` call per row. Keep the current streaming design: the
trait must take batches over bounded channels (backpressure), never a "collect all rows, then
write" signature; that would undo the streaming GeoJSON/Parquet work on `develop`.

**Further compile-time candidates (measure first, take only what pays off):**
- `Filter::Contains { case_insensitive: true }` lowercases the *value* on every eval
  (`v.to_lowercase()` allocates). Lowercase the needle at compile time and use an
  allocation-free ASCII-case-insensitive search where the needle is ASCII.
- `Filter::In` scans a `Vec<String>` linearly. Fine for short lists; compile to a hash set above a
  small threshold.

## After the split (separate work)

- Tree output as JSON Graph Format with structured node `metadata` instead of pre-rendered
  `label`s.
- New backends: TileJSON 3.0.0 (`vector_layers` + `fields`, OSM attribution), JSON Schema,
  Mermaid docs per topic field.
- `lang` in WASM for an in-browser editor preview; becomes possible after steps 5–8, since the
  core has no I/O and no globals.
- Decide whether `--coordinate-precision` should default to 7 (OSM's native 1e-7°).

## Risks

- **Step 5 touches many call sites.** Mechanical, but wide; the global shims keep the pipeline
  unchanged in that PR. Do it right after step 4 so nothing new grows on the old globals.
- **Step 7 is the only semantic step** (merge order in the old `TopicRunner::load`). Snapshots from step 0
  are the guard.
- **Editor coupling.** CLIs and JSON of `osmnexus`/`tree_json` must stay stable through steps 7–9;
  the versioned contract in step 9 makes future changes explicit.
- **Scope creep.** Steps 10 and 11 are optional; stop after 9 if the remaining value is low.
- **Performance.** The only places where this plan *could* cost runtime are cross-crate inlining
  (step 8) and trait dispatch (step 11); both have an explicit check. Everything else either
  happens once per topic at compile time or removes work from the hot path (step 5).
- **Memory.** Peak memory is dominated by node coordinates and way refs in `osm::reader`
  (12.2 GB on Germany), which this plan does not touch; the flat-arena item in `BACKLOG.md` is
  the lever there. The compiled IR is kilobytes to low megabytes, so resolving references into it
  is free in memory terms as long as sets are stored once (step 5) and compile-time inputs are
  dropped after compilation (step 6). Removing the `OnceLock` statics even lets those tables be
  freed, which they never are today.
- **Parallel scaling.** The risks are shared mutable state and reference-count contention, not
  extra work. The plan keeps the IR immutable and borrowed (principle 13, steps 5 and 7), and the
  thread dimension of `bench_sweep.sh` is the check that scaling didn't change.
