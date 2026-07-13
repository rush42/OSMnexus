//! Loading a topic's data from its `topics/<name>/` directory: categories, macros, and
//! sanitizers. Encodes the topic-directory-layout convention (`{node,way,relation}/`,
//! `macros.json`, `sanitizers.json`, plus the config-root-level shared `macros.json`/
//! `sanitizers.json`) — nothing generic-engine lives here, only the disk-I/O side of getting a
//! topic's raw data into the shape `tag_engine` types expect.
//!
//! Layout: a topic organizes its categories into per-kind subfolders directly under the topic dir
//! — `topics/<t>/{node,way,relation}/*.json` (one category per file, id = file stem). Topic-wide
//! macros live in `topics/<t>/macros.json`. Each present kind subfolder becomes one `CategoriesFile`.

use std::collections::HashMap;

use anyhow::Context;
use serde_json::{Map, Value};

use crate::tag_engine::categories::CategoriesFile;
use crate::tag_engine::filter::Filter;
use crate::tag_engine::sanitize::AtomicChain;
use crate::osm::types::ElementKind;

/// Shallow merge, more-specific-scope-wins: every key in `over` overwrites the same key in
/// `base`; keys only in `over` are added. The one primitive behind every "shared/default,
/// specific-scope overrides" cascade in the engine — macros and sanitizers (shared → topic) here,
/// consts and private (topic → category) in `runner.rs`. Works over any map-like type
/// (`HashMap`, `serde_json::Map`) via the standard `IntoIterator`/`Extend` traits, so one
/// implementation covers every level any concept happens to have — there's no single universal
/// shared→topic→category cascade, different concepts stop at different levels (see the doc on
/// `TopicRunner::load`'s macro/sanitizer loading).
pub fn merge<K, V, M>(base: &M, over: &M) -> M
where
    K: Clone,
    V: Clone,
    M: Clone + Extend<(K, V)>,
    for<'a> &'a M: IntoIterator<Item = (&'a K, &'a V)>,
{
    let mut merged = base.clone();
    merged.extend(over.into_iter().map(|(k, v)| (k.clone(), v.clone())));
    merged
}

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

/// Read a topic's own `macros.json` (`{ name: Filter }`) as `Filter` values directly, else an
/// empty map. Distinct from `load_shared_macros` (cross-topic, `<config_root>/macros.json`) —
/// this is one file, holding every topic-local macro. Used to build the raw (pre-`Filter::expand`)
/// macro map a topic's conditions/producers are expanded against.
pub fn load_topic_macros(topic_dir: &std::path::Path) -> anyhow::Result<HashMap<String, Filter>> {
    let path = topic_dir.join("macros.json");
    if path.exists() {
        serde_json::from_str(&std::fs::read_to_string(&path)?)
            .with_context(|| format!("parsing {}", path.display()))
    } else {
        Ok(HashMap::new())
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

/// Load a topic's atomic sanitizer registry: shared (`<config_root>/sanitizers.json`) merged with
/// the topic's own (`<topic>/sanitizers.json`), topic-local winning on name conflict.
/// Self-contained — a `Step` never references another named sanitizer — so nothing here needs a
/// resolution pass; only whatever references an entry by name (`sanitize:`) does.
pub fn load_topic_sanitizers(
    topic_dir: &std::path::Path,
    config_root: &std::path::Path,
) -> anyhow::Result<HashMap<String, AtomicChain>> {
    let read = |path: &std::path::Path| -> anyhow::Result<HashMap<String, AtomicChain>> {
        if path.exists() {
            Ok(serde_json::from_str(&std::fs::read_to_string(path)?)
                .with_context(|| format!("parsing {}", path.display()))?)
        } else {
            Ok(HashMap::new())
        }
    };
    let shared = read(&config_root.join("sanitizers.json"))?;
    let local = read(&topic_dir.join("sanitizers.json"))?;
    Ok(merge(&shared, &local))
}

/// Load shared, cross-topic macros from `<config_root>/macros.json` (`{ name: Filter }`).
/// Referenced by name from any topic's conditions, e.g. `{ "macro": "standard_exclude" }`.
pub fn load_shared_macros(config_root: &std::path::Path) -> anyhow::Result<HashMap<String, Filter>> {
    let path = config_root.join("macros.json");
    if path.exists() {
        serde_json::from_str(&std::fs::read_to_string(&path)?)
            .with_context(|| format!("parsing {}", path.display()))
    } else {
        Ok(HashMap::new())
    }
}

/// Load shared, cross-topic named rule tables from `<config_root>/producers.json`
/// (`{ name: <producer JSON> }`, e.g. the `road` classifier) as raw JSON — kept raw (not
/// deserialized into `Producer`) since `inline_shared_producers` substitutes them into a topic's
/// own raw JSON before anything is deserialized.
pub fn load_shared_producers(config_root: &std::path::Path) -> anyhow::Result<Map<String, Value>> {
    let path = config_root.join("producers.json");
    if path.exists() {
        serde_json::from_str(&std::fs::read_to_string(&path)?)
            .with_context(|| format!("parsing {}", path.display()))
    } else {
        Ok(Map::new())
    }
}

/// Recursively replace every `{ "shared": "<name>", ... }` object in `value` with `shared[name]`'s
/// own producer JSON — any sibling keys the referencing site set (`from`/`consts`) override the
/// same keys in the shared table's JSON. This is the entire "shared classifier" mechanism: it
/// happens once here, at topic-directory-read time, on raw JSON — the same treatment shared
/// macros/sanitizers get (see `merge`) — so `Producer`'s own `Deserialize` never has to represent
/// a shared-table reference at all. Errors loudly on an unknown name, same as a producer-library
/// name miss (`spec::resolve_output_entry`).
pub fn inline_shared_producers(value: Value, shared: &Map<String, Value>) -> anyhow::Result<Value> {
    Ok(match value {
        Value::Object(mut obj) => match obj.get("shared").cloned() {
            Some(Value::String(name)) => {
                obj.remove("shared");
                let mut inlined = shared.get(&name)
                    .ok_or_else(|| anyhow::anyhow!("no shared producer named '{name}' in <config_root>/producers.json"))?
                    .clone();
                if let Value::Object(inlined_obj) = &mut inlined {
                    inlined_obj.extend(obj);
                }
                inline_shared_producers(inlined, shared)?
            }
            Some(other) => anyhow::bail!("`shared` must be a string naming a producer, got {other}"),
            None => Value::Object(obj.into_iter()
                .map(|(k, v)| Ok((k, inline_shared_producers(v, shared)?)))
                .collect::<anyhow::Result<_>>()?),
        },
        Value::Array(items) => Value::Array(items.into_iter()
            .map(|v| inline_shared_producers(v, shared))
            .collect::<anyhow::Result<_>>()?),
        other => other,
    })
}
