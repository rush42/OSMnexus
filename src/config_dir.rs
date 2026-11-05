//! Shell side of config loading: the only place that reads a config directory from disk. Walks it
//! once into `ConfigSources` (every `*.json` file plus every directory), builds the `CompileEnv`,
//! and hands both to the pure core (`Topic::load`, the overlap linter). See
//! `boundaries_plan.md`, step 6.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context;

use crate::lang::env::CompileEnv;
use crate::topic::sources::ConfigSources;
use crate::topic::Topic;

/// A config directory read into memory, plus its compile environment.
pub struct Config {
    pub sources: ConfigSources,
    pub env: Arc<CompileEnv>,
}

impl Config {
    /// Read `root` and build the compile environment for the given traffic side.
    pub fn read(root: impl AsRef<Path>, left_hand_traffic: bool) -> anyhow::Result<Self> {
        let sources = read_sources(root.as_ref())?;
        let env = Arc::new(CompileEnv::from_sources(&sources, left_hand_traffic)?);
        Ok(Config { sources, env })
    }

    /// The default config (`paths::config_root()`, right-hand traffic) — for tests, benches and
    /// tools that don't take a `--config-dir`.
    pub fn default_root() -> anyhow::Result<Self> {
        Self::read(crate::paths::config_root(), false)
    }

    /// Load every topic, ready to run (see `prepare`).
    pub fn load_all(&self, tree_max_depth: usize) -> anyhow::Result<Vec<Topic>> {
        Ok(Topic::load_all(&self.sources, &self.env, tree_max_depth)?.into_iter().map(prepare).collect())
    }

    /// Load one topic, ready to run (see `prepare`).
    pub fn load(&self, name: &str, tree_max_depth: usize) -> anyhow::Result<Topic> {
        Topic::load(&self.sources, &self.env, name, tree_max_depth).map(prepare)
    }
}

/// The run-side finishing step for a freshly loaded topic: probe which kinds can skip untagged
/// elements (`topic::pipeline::probe_skip_untagged`). A topic used without this is still correct,
/// just never skips.
pub fn prepare(mut topic: Topic) -> Topic {
    let kinds = crate::topic::pipeline::probe_skip_untagged(&topic);
    topic.set_skip_untagged(kinds);
    topic
}

/// Walk `root` recursively: every directory is registered (so an empty kind folder still counts
/// as present, as before) and every `*.json` file is read. Other files are ignored; nothing but
/// JSON is ever read from a config directory.
pub fn read_sources(root: &Path) -> anyhow::Result<ConfigSources> {
    let mut sources = ConfigSources::new(root);
    let mut stack: Vec<(PathBuf, String)> = vec![(root.to_path_buf(), String::new())];
    while let Some((dir, rel)) = stack.pop() {
        let entries = std::fs::read_dir(&dir).with_context(|| format!("reading {}", dir.display()))?;
        for entry in entries {
            let entry = entry.with_context(|| format!("reading {}", dir.display()))?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let child_rel = crate::topic::sources::join(&rel, &name);
            let path = entry.path();
            // `metadata` (not `file_type`) so symlinked topic folders/files behave like the old
            // `is_dir`/`exists` checks, which followed symlinks.
            let meta = std::fs::metadata(&path).with_context(|| format!("reading {}", path.display()))?;
            if meta.is_dir() {
                sources.insert_dir(child_rel.clone());
                stack.push((path, child_rel));
            } else if name.ends_with(".json") {
                let contents =
                    std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
                sources.insert_file(child_rel, contents);
            }
        }
    }
    Ok(sources)
}
