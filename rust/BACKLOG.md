# Backlog

Deferred ideas / nice-to-haves for the Rust pipeline. Not blocking anything.

- **`--threads N` / `--no-parallel` flag.** Add an explicit parallelism knob to `Config`
  (config.rs) that calls `rayon::ThreadPoolBuilder::new().num_threads(n).build_global()` at
  startup in main.rs. Today the only control is the `RAYON_NUM_THREADS` env var; rayon otherwise
  defaults to the logical CPU count for the reader's Pass A/B/C `par_iter`/`par_map_reduce`. A CLI
  flag would make constraining CPU/memory during large (Germany) runs first-class.
