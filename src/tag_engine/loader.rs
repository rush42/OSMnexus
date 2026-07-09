//! Loading categories and shared macros from the `topics/` tree.
//!
//! Layout: a topic organizes its categories into per-kind subfolders directly under the topic dir
//! — `topics/<t>/{node,way,relation}/*.json` (one category per file, id = file stem). Topic-wide
//! macros live in `topics/<t>/macros.json`. Each present kind subfolder becomes one `CategoriesFile`.

use std::collections::HashMap;

use anyhow::Context;
use serde_json::{Map, Value};

use crate::tag_engine::categories::CategoriesFile;
use crate::tag_engine::filter::Filter;
use crate::osm::types::ElementKind;

/// Load a topic's per-kind category sets from its directory. Reads the optional topic-wide
/// `macros.json`, then for each of `node`/`way`/`relation` that exists as a subfolder, loads its
/// `*.json` category files into a `CategoriesFile` (seeded with the topic macros). Only present
/// kinds appear in the returned map — a topic with just a `way/` folder yields a single entry.
pub fn load_topic_categories(
    topic_dir: &std::path::Path,
) -> anyhow::Result<HashMap<ElementKind, CategoriesFile>> {
    let macros = read_macros(&topic_dir.join("macros.json"))?;

    let mut out = HashMap::new();
    for kind in ElementKind::ALL {
        let dir = topic_dir.join(kind.subdir());
        if dir.is_dir() {
            let cats = load_categories_dir(&dir, &macros)
                .with_context(|| format!("loading {}", dir.display()))?;
            out.insert(kind, cats);
        }
    }
    Ok(out)
}

/// Read a macros JSON file (`{ name: Filter }`) if it exists, else an empty map.
fn read_macros(path: &std::path::Path) -> anyhow::Result<Map<String, Value>> {
    if path.exists() {
        serde_json::from_str(&std::fs::read_to_string(path)?)
            .with_context(|| format!("parsing {}", path.display()))
    } else {
        Ok(Map::new())
    }
}

/// Load all `*.json` category files (sorted) in one kind subfolder into a `CategoriesFile`, seeding
/// its macro namespace with `macros` (the topic-wide macros). The category `id` is the file stem.
pub fn load_categories_dir(
    dir: &std::path::Path,
    macros: &Map<String, Value>,
) -> anyhow::Result<CategoriesFile> {
    let mut entries: Vec<_> = std::fs::read_dir(dir)?
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().and_then(|s| s.to_str()) == Some("json"))
        .collect();
    entries.sort_by_key(|e| e.file_name());

    let mut categories: Vec<Value> = Vec::with_capacity(entries.len());
    for entry in entries {
        let stem = entry.path().file_stem().unwrap().to_string_lossy().to_string();
        let mut obj: Value = serde_json::from_str(&std::fs::read_to_string(entry.path())?)
            .with_context(|| format!("parsing {}", entry.path().display()))?;
        if let Value::Object(ref mut map) = obj {
            map.insert("id".to_owned(), Value::String(stem));
        }
        categories.push(obj);
    }

    let combined = Value::Object(Map::from_iter([
        ("macros".to_owned(), Value::Object(macros.clone())),
        ("categories".to_owned(), Value::Array(categories)),
    ]));
    Ok(serde_json::from_value(combined)?)
}

/// Load shared, cross-topic macros from `topics/_shared/macros/<name>.json` (one Filter per
/// file, macro name = file stem). Referenced by name from any topic's conditions, e.g.
/// `{ "macro": "standard_exclude" }`. `shared_dir` is `topics/_shared`; only its `macros/`
/// subdirectory holds Filter macros — the data libraries (sanitizers.json, value_sets.json,
/// classifiers/) live at the `_shared/` root and are loaded explicitly elsewhere.
pub fn load_shared_macros(shared_dir: &std::path::Path) -> anyhow::Result<HashMap<String, Filter>> {
    let mut macros = HashMap::new();
    let dir = shared_dir.join("macros");
    if !dir.exists() {
        return Ok(macros);
    }
    for entry in std::fs::read_dir(&dir)? {
        let path = entry?.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let name = path.file_stem().unwrap().to_string_lossy().to_string();
        let filter: Filter = serde_json::from_str(&std::fs::read_to_string(&path)?)
            .with_context(|| format!("parsing shared macro {}", path.display()))?;
        macros.insert(name, filter);
    }
    Ok(macros)
}
