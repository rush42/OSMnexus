//! Loading categories and shared macros from the `topics/` tree.

use std::collections::HashMap;

use anyhow::Context;
use serde_json::{Map, Value};

use crate::classify::categories::CategoriesFile;
use crate::classify::filter::Filter;

/// Load a topic's categories from its `categories/` directory.
/// Reads `macros.json` (optional) + all other `*.json` files (sorted), injecting the
/// category `id` from each file stem.
pub fn load_categories_from_dir(dir: &std::path::Path) -> anyhow::Result<CategoriesFile> {
    let macros_path = dir.join("macros.json");
    let macros: Map<String, Value> = if macros_path.exists() {
        serde_json::from_str(&std::fs::read_to_string(&macros_path)?)
            .with_context(|| format!("parsing {}", macros_path.display()))?
    } else {
        Map::new()
    };

    let mut entries: Vec<_> = std::fs::read_dir(dir)?
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.path().extension().and_then(|s| s.to_str()) == Some("json")
                && e.file_name() != std::ffi::OsStr::new("macros.json")
        })
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
        ("macros".to_owned(), Value::Object(macros)),
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
