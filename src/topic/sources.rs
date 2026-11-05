//! `ConfigSources`: a config directory as data — every JSON file's contents keyed by its path
//! relative to the config root, plus the set of directories. The core (`topic::load`,
//! `topic::model`, `categorize::linter`) reads config only through this, never the filesystem; the
//! shell (`config_dir`) builds it once by walking the directory (see `boundaries_plan.md`,
//! step 6). That keeps compilation a pure function of its inputs: testable from in-memory
//! sources, and usable where there is no filesystem (e.g. an in-browser editor).
//!
//! Relative paths use `/` as separator and no leading `./`; the root itself is `""`.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default)]
pub struct ConfigSources {
    /// Only used to render paths in error messages, so they read exactly like the on-disk path.
    root: PathBuf,
    files: BTreeMap<String, String>,
    dirs: BTreeSet<String>,
}

/// Join two relative paths (`""` is the root).
pub fn join(dir: &str, name: &str) -> String {
    if dir.is_empty() { name.to_owned() } else { format!("{dir}/{name}") }
}

impl ConfigSources {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        ConfigSources { root: root.into(), files: BTreeMap::new(), dirs: BTreeSet::new() }
    }

    /// Add a file's contents at `rel`, registering its parent directories too.
    pub fn insert_file(&mut self, rel: impl Into<String>, contents: String) {
        let rel = rel.into();
        if let Some((parent, _)) = rel.rsplit_once('/') {
            self.insert_dir(parent.to_owned());
        }
        self.files.insert(rel, contents);
    }

    /// Register a directory (and its parents), so an empty directory still counts as present.
    pub fn insert_dir(&mut self, rel: impl Into<String>) {
        let mut rel: String = rel.into();
        while !rel.is_empty() && self.dirs.insert(rel.clone()) {
            match rel.rsplit_once('/') {
                Some((parent, _)) => rel = parent.to_owned(),
                None => break,
            }
        }
    }

    /// `rel` rendered as the on-disk path, for error messages.
    pub fn path(&self, rel: &str) -> PathBuf {
        self.root.join(rel)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The contents of the file at `rel`, if present.
    pub fn read(&self, rel: &str) -> Option<&str> {
        self.files.get(rel).map(String::as_str)
    }

    pub fn is_dir(&self, rel: &str) -> bool {
        rel.is_empty() || self.dirs.contains(rel)
    }

    /// Names of the direct subdirectories of `rel`, sorted.
    pub fn subdirs(&self, rel: &str) -> Vec<String> {
        self.dirs
            .iter()
            .filter_map(|d| child_name(rel, d))
            .map(str::to_owned)
            .collect()
    }

    /// `(file name, contents)` of every direct `*.json` child of `rel`, sorted by file name.
    pub fn json_files(&self, rel: &str) -> Vec<(&str, &str)> {
        self.files
            .iter()
            .filter_map(|(path, contents)| child_name(rel, path).map(|name| (name, contents.as_str())))
            .filter(|(name, _)| name.ends_with(".json"))
            .collect()
    }
}

/// `path`'s name if it is a direct child of `dir`.
fn child_name<'a>(dir: &str, path: &'a str) -> Option<&'a str> {
    let rest = if dir.is_empty() { path } else { path.strip_prefix(dir)?.strip_prefix('/')? };
    (!rest.is_empty() && !rest.contains('/')).then_some(rest)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn listing() {
        let mut s = ConfigSources::new("/cfg");
        s.insert_file("macros.json", "{}".into());
        s.insert_file("bikelanes/topic.json", "{}".into());
        s.insert_file("bikelanes/way/b.json", "{}".into());
        s.insert_file("bikelanes/way/a.json", "{}".into());
        s.insert_file("bikelanes/way/README.md", "".into());
        s.insert_dir("roads/node");
        assert_eq!(s.subdirs(""), vec!["bikelanes", "roads"]);
        assert!(s.is_dir("roads/node"));
        assert!(!s.is_dir("roads/way"));
        let names: Vec<_> = s.json_files("bikelanes/way").into_iter().map(|(n, _)| n).collect();
        assert_eq!(names, vec!["a.json", "b.json"]);
        assert_eq!(s.json_files("").len(), 1);
        assert_eq!(s.read("bikelanes/topic.json"), Some("{}"));
        assert_eq!(s.path("bikelanes/topic.json"), PathBuf::from("/cfg/bikelanes/topic.json"));
    }
}
