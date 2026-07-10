//! Loading shared, named classifiers from `topics/_shared/classifiers/`.

use std::collections::HashMap;
use std::sync::OnceLock;

use crate::tag_engine::producer::classifier::Classifier;

/// Shared, named classifiers loaded once from `topics/_shared/classifiers/<name>.json`
/// (name = file stem). Referenced from data via a `Classify`-style producer's `{ "shared": "<name>" }`,
/// so a rule table (e.g. the `road` classification) can be reused across topics without duplication.
fn shared_classifiers() -> &'static HashMap<String, Classifier> {
    static CLASSIFIERS: OnceLock<HashMap<String, Classifier>> = OnceLock::new();
    CLASSIFIERS.get_or_init(|| {
        let dir = crate::paths::shared_dir().join("classifiers");
        let dir = dir.display().to_string();
        let mut map = HashMap::new();
        let entries = std::fs::read_dir(&dir).unwrap_or_else(|e| panic!("reading {dir}: {e}"));
        for entry in entries {
            let path = entry.unwrap().path();
            if path.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            let name = path.file_stem().unwrap().to_string_lossy().to_string();
            let raw = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
            let classifier = serde_json::from_str(&raw)
                .unwrap_or_else(|e| panic!("parsing {}: {e}", path.display()));
            map.insert(name, classifier);
        }
        map
    })
}

/// The shared classifier registered under `name`, panicking if undefined (a config error).
pub fn shared_classifier(name: &str) -> &'static Classifier {
    shared_classifiers()
        .get(name)
        .unwrap_or_else(|| panic!("no shared classifier named '{name}' in topics/_shared/classifiers"))
}
