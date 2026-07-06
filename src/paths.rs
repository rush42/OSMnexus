//! The active **config directory** — a self-contained folder of topics plus a `_shared/` library
//! (macros, sanitizers, value_sets.json, classifiers/). Selected at startup via `--config-dir` and
//! read here as a process-global, so the topic loader and the `_shared` singletons (`value_sets`,
//! shared classifiers) all resolve against the same root without threading a path through every call.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

static CONFIG_ROOT: OnceLock<PathBuf> = OnceLock::new();

/// Set the active config directory. Called once at startup from `--config-dir`; ignored if already
/// set (first caller wins). Tests may call it to point at a specific config folder.
pub fn set_config_root(path: impl Into<PathBuf>) {
    let _ = CONFIG_ROOT.set(path.into());
}

/// The active config directory. Resolution order: an explicit `set_config_root`, then the
/// `TILDA_CONFIG_DIR` env var (lets `check_overlaps` / tests target any config), then the default
/// `<crate>/configs/tilda`.
pub fn config_root() -> PathBuf {
    if let Some(p) = CONFIG_ROOT.get() {
        return p.clone();
    }
    if let Some(p) = std::env::var_os("TILDA_CONFIG_DIR") {
        return PathBuf::from(p);
    }
    Path::new(env!("CARGO_MANIFEST_DIR")).join("configs/tilda")
}

/// The config's `_shared/` library directory (`<config_root>/_shared`).
pub fn shared_dir() -> PathBuf {
    config_root().join("_shared")
}
