# Build guidance

`cargo build --release` is slow by design (`lto = "thin"`, `codegen-units = 1` in
[Cargo.toml](Cargo.toml)) — it recompiles the whole crate from scratch on every change and
takes ~60s+ even for a one-line edit. Don't use it while iterating.

For day-to-day compile checks and running binaries during development, use:

```
cargo check                          # fastest, no codegen — use this first
cargo build --profile dev-fast --bin osmnexus   # ~0.5s incremental rebuilds, runnable binary
cargo build --profile dev-fast --bin tree_json
```

Only build `--release` when producing the actual optimized binary is required (e.g. the
Docker image, or benchmarking `--bin osmnexus` against real data volumes where opt-level
matters).
