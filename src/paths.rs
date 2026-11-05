//! Where the default config directory is — a self-contained folder of topic subdirectories plus a
//! shared library of flat files at its root (`macros.json`, `sanitizers.json`, `producers.json`,
//! `value_sets.json`, `units.json`). Shell-side only: binaries that take `--config-dir` (or a
//! config-dir argument) pass it to `config_dir::Config::read` directly; this is the fallback for
//! tests, benches and tools that don't. No process-global state: the config is read once into
//! `ConfigSources` and passed explicitly from there on.

use std::path::{Path, PathBuf};

/// The default config directory: the `TILDA_CONFIG_DIR` env var (lets `check_overlaps` / tests
/// target any config), else `<crate>/configs/tilda`.
pub fn config_root() -> PathBuf {
    if let Some(p) = std::env::var_os("TILDA_CONFIG_DIR") {
        return PathBuf::from(p);
    }
    Path::new(env!("CARGO_MANIFEST_DIR")).join("configs/tilda")
}
