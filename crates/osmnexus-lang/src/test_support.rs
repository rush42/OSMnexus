//! Test-only helpers: read a real config directory from the repository. The crate itself never
//! touches the filesystem (that's the shell's `config_dir`); tests need real configs, so this is
//! a small copy of the shell's directory walker, compiled only under `cfg(test)`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::lang::env::CompileEnv;
use crate::topic::sources::{join, ConfigSources};
use crate::topic::Topic;

/// The repository root (two levels above this crate's manifest).
pub fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

pub struct Config {
    pub sources: ConfigSources,
    pub env: Arc<CompileEnv>,
}

impl Config {
    pub fn read(root: impl AsRef<Path>, left_hand_traffic: bool) -> anyhow::Result<Self> {
        let sources = read_sources(root.as_ref())?;
        let env = Arc::new(CompileEnv::from_sources(&sources, left_hand_traffic)?);
        Ok(Config { sources, env })
    }

    /// `TILDA_CONFIG_DIR`, else the repository's `configs/tilda`.
    pub fn default_root() -> anyhow::Result<Self> {
        let root = std::env::var_os("TILDA_CONFIG_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| repo_root().join("configs/tilda"));
        Self::read(root, false)
    }

    pub fn load(&self, name: &str, tree_max_depth: usize) -> anyhow::Result<Topic> {
        Topic::load(&self.sources, &self.env, name, tree_max_depth)
    }
}

fn read_sources(root: &Path) -> anyhow::Result<ConfigSources> {
    let mut sources = ConfigSources::new(root);
    let mut stack: Vec<(PathBuf, String)> = vec![(root.to_path_buf(), String::new())];
    while let Some((dir, rel)) = stack.pop() {
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let child_rel = join(&rel, &name);
            let path = entry.path();
            if std::fs::metadata(&path)?.is_dir() {
                sources.insert_dir(child_rel.clone());
                stack.push((path, child_rel));
            } else if name.ends_with(".json") {
                sources.insert_file(child_rel, std::fs::read_to_string(&path)?);
            }
        }
    }
    Ok(sources)
}
