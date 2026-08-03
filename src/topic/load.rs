//! Loading a topic's data from its `topics/<name>/` directory: categories, macros, and
//! sanitizers. Encodes the topic-directory-layout convention (`{node,way,relation}/`,
//! `macros.json`, `sanitizers.json`, plus the config-root-level shared `macros.json`/
//! `sanitizers.json`) — nothing generic-engine lives here, only the disk-I/O side of getting a
//! topic's raw data into the shape `lang`/`categorize` types expect.
//!
//! Layout: a topic organizes its categories into per-kind subfolders directly under the topic dir
//! — `topics/<t>/{node,way,relation}/*.json` (one category per file, id = file stem). Topic-wide
//! macros live in `topics/<t>/macros.json`. Each present kind subfolder becomes one `CategoriesFile`.
//!
//! Also home to the two generic `serde_json::Value`-tree rewrites every raw topic JSON document
//! (`topic.json`, `transforms.json`, `producers.json`, each category file) is run through before
//! any of it is deserialized into a `Filter`/`Producer`/`Rule`/`CategoryDef`: `inline_macro_refs`
//! (substitutes `{"macro": "<name>"}` with the macro's own, recursively-inlined JSON) and
//! `inline_sanitize_refs` (substitutes a bare `"sanitize": "<name>"` string with the resolved
//! chain's own JSON, via `Sanitizer`'s `Serialize` impl). Both are purely structural Value
//! rewrites — they don't know or care which `Filter`/`Producer` variant a match sits inside, the
//! same treatment `inline_shared_producers` below already gives `{"shared": "<name>"}`. After both
//! passes, a `Filter`/`Producer`/`Rule`/`CategoryDef`'s own `Deserialize` never has to represent an
//! unresolved reference at all — there's only ever the one (resolved) tier of each type.

use std::collections::HashMap;

use anyhow::Context;
use serde_json::{Map, Value};

use crate::categorize::categories::CategoriesFile;
use crate::lang::filter::Filter;
use crate::lang::sanitize::{resolve_named_sanitizer, Sanitizer};
use crate::parser::parse_sanitize_chain;
use crate::osm::types::ElementKind;
use crate::topic::spec::TransformsSpec;

/// Shallow merge, more-specific-scope-wins: every key in `over` overwrites the same key in
/// `base`; keys only in `over` are added. The one primitive behind every "shared/default,
/// specific-scope overrides" cascade in the engine — macros and sanitizers (shared → topic) here,
/// annotate and private (topic → category) in `runner.rs`. Works over any map-like type
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

// ── Macro/sanitizer JSON-tree inlining ──────────────────────────────────────────

/// Recursively replace every `{"macro": "<name>"}` object in `value` with `macros[name]`'s own
/// (recursively inlined) JSON — the load-time pass that makes an unexpanded macro structurally
/// impossible by the time `Filter`/`Producer` deserialize. `stack` cycle-checks: entering the same
/// macro name twice while still inside its own expansion is a contradictory (infinite) definition,
/// a hard load-time error, same as an unknown name.
pub fn inline_macro_refs(value: Value, macros: &Map<String, Value>, stack: &mut Vec<String>) -> anyhow::Result<Value> {
    Ok(match value {
        Value::Object(mut obj) if obj.len() == 1 && obj.contains_key("macro") => {
            let name = match obj.remove("macro") {
                Some(Value::String(name)) => name,
                _ => anyhow::bail!("`macro` must be a string"),
            };
            if stack.iter().any(|n| n == &name) {
                stack.push(name);
                anyhow::bail!("cyclic macro definition: {}", stack.join(" -> "));
            }
            let def = macros.get(&name).cloned()
                .ok_or_else(|| anyhow::anyhow!("unknown macro: '{name}'"))?;
            stack.push(name);
            let expanded = inline_macro_refs(def, macros, stack)?;
            stack.pop();
            expanded
        }
        Value::Object(obj) => Value::Object(obj.into_iter()
            .map(|(k, v)| Ok((k, inline_macro_refs(v, macros, stack)?)))
            .collect::<anyhow::Result<_>>()?),
        Value::Array(items) => Value::Array(items.into_iter()
            .map(|v| inline_macro_refs(v, macros, stack))
            .collect::<anyhow::Result<_>>()?),
        other => other,
    })
}

/// Recursively replace every `"sanitize": "<name>"` string in `value` with the resolved chain's
/// own JSON (`Sanitizer`'s `Serialize` impl) — the load-time pass that makes an unresolved named
/// sanitizer structurally impossible by the time `Filter`/`Producer` deserialize `sanitize:`
/// straight into `Vec<Sanitizer>`. `sanitize:` must always be authored as a name — every sanitizer
/// chain has exactly one definition, in `sanitizers.json`, so there's no such thing as an anonymous
/// inline chain; a non-string `sanitize` value is a load-time error.
pub fn inline_sanitize_refs(value: Value, sanitizers: &HashMap<String, Vec<Sanitizer>>) -> anyhow::Result<Value> {
    Ok(match value {
        Value::Object(obj) => Value::Object(obj.into_iter()
            .map(|(k, v)| {
                let v = if k == "sanitize" {
                    match v {
                        Value::String(name) => serde_json::to_value(resolve_named_sanitizer(&name, sanitizers))?,
                        other => anyhow::bail!(
                            "`sanitize` must be a named reference (a string), not an inline chain: {other}"
                        ),
                    }
                } else {
                    inline_sanitize_refs(v, sanitizers)?
                };
                Ok((k, v))
            })
            .collect::<anyhow::Result<_>>()?),
        Value::Array(items) => Value::Array(items.into_iter()
            .map(|v| inline_sanitize_refs(v, sanitizers))
            .collect::<anyhow::Result<_>>()?),
        other => other,
    })
}

/// Run both inlining passes, in order (a macro body can itself carry a `sanitize:` reference, so
/// macros must be inlined first). The one entry point every raw topic JSON document goes through
/// before its first typed `Deserialize` call.
pub fn resolve_refs(value: Value, macros: &Map<String, Value>, sanitizers: &HashMap<String, Vec<Sanitizer>>) -> anyhow::Result<Value> {
    let value = inline_macro_refs(value, macros, &mut Vec::new())?;
    inline_sanitize_refs(value, sanitizers)
}

// ── Macros ───────────────────────────────────────────────────────────────────

/// Read a macros JSON file (`{ name: <Filter JSON> }`) if it exists, else an empty map. Raw —
/// macro bodies may still reference other macros (folded in by `inline_macro_refs`).
fn read_macros(path: &std::path::Path) -> anyhow::Result<Map<String, Value>> {
    if path.exists() {
        serde_json::from_str(&std::fs::read_to_string(path)?)
            .with_context(|| format!("parsing {}", path.display()))
    } else {
        Ok(Map::new())
    }
}

/// Read a topic's own `macros.json`, raw. Distinct from `load_shared_macros` (cross-topic,
/// `<config_root>/macros.json`) — this is one file, holding every topic-local macro.
pub fn load_topic_macros(topic_dir: &std::path::Path) -> anyhow::Result<Map<String, Value>> {
    read_macros(&topic_dir.join("macros.json"))
}

/// Read shared, cross-topic macros from `<config_root>/macros.json`, raw. Referenced by name from
/// any topic's conditions, e.g. `{ "macro": "standard_exclude" }`.
pub fn load_shared_macros(config_root: &std::path::Path) -> anyhow::Result<Map<String, Value>> {
    read_macros(&config_root.join("macros.json"))
}

/// Recursively macro-inline (and sanitizer-inline) every entry in `raw_macros` against itself, so
/// a macro body referencing another macro is expanded too. The result is what `inline_macro_refs`
/// needs elsewhere (a fully macro-free JSON per name) — see `topic::runner::TopicRunner::load`.
pub fn resolve_macros(raw_macros: &Map<String, Value>, sanitizers: &HashMap<String, Vec<Sanitizer>>) -> anyhow::Result<Map<String, Value>> {
    raw_macros.iter()
        .map(|(name, def)| {
            let expanded = inline_macro_refs(def.clone(), raw_macros, &mut vec![name.clone()])?;
            Ok((name.clone(), inline_sanitize_refs(expanded, sanitizers)?))
        })
        .collect()
}

/// Read a topic's own `transforms.json` — its whole transform pipeline, explicit about where
/// `exclude_condition` is evaluated (see `TransformsSpec`) — if present, fully macro/sanitizer
/// resolved. `None` when the topic still uses the legacy `input_transforms`/`split_sides`
/// `topic.json` keys instead; `TopicRunner::load` falls back to synthesizing an equivalent
/// pipeline from those in that case. Topic-local only, no shared config-root variant (unlike
/// `macros.json`/`sanitizers.json`) — nothing shares a transform pipeline across topics today.
pub fn load_topic_transforms(
    topic_dir: &std::path::Path,
    resolved_macros: &Map<String, Value>,
    sanitizers: &HashMap<String, Vec<Sanitizer>>,
) -> anyhow::Result<Option<TransformsSpec>> {
    let path = topic_dir.join("transforms.json");
    if !path.exists() {
        return Ok(None);
    }
    let raw: Value = serde_json::from_str(&std::fs::read_to_string(&path)?)
        .with_context(|| format!("parsing {}", path.display()))?;
    let resolved = resolve_refs(raw, resolved_macros, sanitizers)
        .with_context(|| format!("resolving {}", path.display()))?;
    Ok(Some(serde_json::from_value(resolved).with_context(|| format!("parsing {}", path.display()))?))
}

// ── Categories ───────────────────────────────────────────────────────────────

/// Load a topic's per-kind category sets from its directory, fully macro/sanitizer-resolved.
/// `resolved_macros` is the topic's macro table with every reference already inlined (see
/// `resolve_macros`); `macros_filter` is the same table deserialized to `Filter` (needed
/// separately by `CategoriesFile`/`build_order` — an `excludes` entry names a macro directly, not
/// through a condition, so it's never touched by `inline_macro_refs`). Only present kinds appear
/// in the returned map — a topic with just a `way/` folder yields a single entry.
pub fn load_topic_categories(
    topic_dir: &std::path::Path,
    resolved_macros: &Map<String, Value>,
    macros_filter: &HashMap<String, Filter>,
    sanitizers: &HashMap<String, Vec<Sanitizer>>,
) -> anyhow::Result<HashMap<ElementKind, CategoriesFile>> {
    let mut out = HashMap::new();
    for kind in ElementKind::ALL {
        let dir = topic_dir.join(kind.subdir());
        if dir.is_dir() {
            let cats = load_categories_dir(&dir, resolved_macros, macros_filter, sanitizers)
                .with_context(|| format!("loading {}", dir.display()))?;
            out.insert(kind, cats);
        }
    }
    Ok(out)
}

/// Load all `*.json` category files (sorted) in one kind subfolder into a `CategoriesFile`. Each
/// file is either a single category (id = file stem, as before) or a *family*: a base object
/// carrying shared `condition`/`excludes`/`defaults`/`outputs` plus a `categories` array of
/// variants, each expanded into its own category by `expand_family` (id = `<stem>_<name>`).
pub fn load_categories_dir(
    dir: &std::path::Path,
    resolved_macros: &Map<String, Value>,
    macros_filter: &HashMap<String, Filter>,
    sanitizers: &HashMap<String, Vec<Sanitizer>>,
) -> anyhow::Result<CategoriesFile> {
    let mut entries: Vec<_> = std::fs::read_dir(dir)?
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().and_then(|s| s.to_str()) == Some("json"))
        .collect();
    entries.sort_by_key(|e| e.file_name());

    let mut categories: Vec<Value> = Vec::new();
    for entry in entries {
        let stem = entry.path().file_stem().unwrap().to_string_lossy().to_string();
        let obj: Value = serde_json::from_str(&std::fs::read_to_string(entry.path())?)
            .with_context(|| format!("parsing {}", entry.path().display()))?;
        categories.extend(expand_family(&stem, obj)
            .with_context(|| format!("expanding {}", entry.path().display()))?);
    }

    let combined = Value::Object(Map::from_iter([("categories".to_owned(), Value::Array(categories))]));
    let resolved = resolve_refs(combined, resolved_macros, sanitizers)
        .with_context(|| format!("resolving {}", dir.display()))?;
    CategoriesFile::from_categories_json(resolved, macros_filter.clone())
}

/// Expand one category file's raw JSON into one or more flat category objects, each carrying its
/// own `id`. A plain object (no `categories` key) is a single category, unchanged apart from
/// getting `id: <stem>` — today's behavior. A `categories` array turns the file into a *family*:
/// the object's own `condition`/`excludes`/`defaults`/`outputs` become the shared base, and each
/// array entry is a variant merged over that base — `condition` ANDed, `excludes` unioned,
/// `defaults`/`outputs` key-merged (variant wins) — with `id: <stem>_<variant name>`, so ids stay
/// identical to what today's one-category-per-file layout would produce for the same names.
fn expand_family(stem: &str, mut obj: Value) -> anyhow::Result<Vec<Value>> {
    let Some(map) = obj.as_object_mut() else {
        anyhow::bail!("category file must contain a JSON object");
    };
    let Some(Value::Array(variants)) = map.remove("categories") else {
        // Not a family: restore nothing removed, just stamp the id.
        map.insert("id".to_owned(), Value::String(stem.to_owned()));
        return Ok(vec![obj]);
    };

    let base = map; // remaining keys after removing `categories`: condition/excludes/defaults/outputs/...
    let base_condition = base.get("condition").cloned();
    let base_excludes: Vec<Value> =
        base.get("excludes").and_then(Value::as_array).cloned().unwrap_or_default();
    let base_defaults = base.get("defaults").and_then(Value::as_object).cloned().unwrap_or_default();
    let base_outputs = base.get("outputs").and_then(Value::as_object).cloned().unwrap_or_default();

    variants.into_iter().map(|variant| {
        let mut variant = variant;
        let Some(vmap) = variant.as_object_mut() else {
            anyhow::bail!("each family variant must be a JSON object");
        };
        let name = match vmap.remove("name") {
            Some(Value::String(name)) => name,
            _ => anyhow::bail!("each family variant needs a string `name`"),
        };

        let condition = match (base_condition.clone(), vmap.remove("condition")) {
            (Some(b), Some(v)) => Value::Object(Map::from_iter([
                ("and".to_owned(), Value::Array(vec![b, v])),
            ])),
            (Some(b), None) => b,
            (None, Some(v)) => v,
            (None, None) => anyhow::bail!("variant '{name}' has no condition (and family has no base condition)"),
        };

        let mut excludes = base_excludes.clone();
        if let Some(Value::Array(extra)) = vmap.remove("excludes") {
            for e in extra {
                if !excludes.contains(&e) {
                    excludes.push(e);
                }
            }
        }

        let mut defaults = base_defaults.clone();
        if let Some(Value::Object(over)) = vmap.remove("defaults") {
            defaults = merge(&defaults, &over);
        }
        let mut outputs = base_outputs.clone();
        if let Some(Value::Object(over)) = vmap.remove("outputs") {
            outputs = merge(&outputs, &over);
        }

        let mut result = Map::new();
        result.insert("id".to_owned(), Value::String(format!("{stem}_{name}")));
        result.insert("condition".to_owned(), condition);
        if !excludes.is_empty() {
            result.insert("excludes".to_owned(), Value::Array(excludes));
        }
        if !defaults.is_empty() {
            result.insert("defaults".to_owned(), Value::Object(defaults));
        }
        if !outputs.is_empty() {
            result.insert("outputs".to_owned(), Value::Object(outputs));
        }
        // Any other variant-specific keys (forward-compatible) pass through untouched.
        for (k, v) in vmap.iter() {
            result.entry(k.clone()).or_insert_with(|| v.clone());
        }
        Ok(Value::Object(result))
    }).collect()
}

// ── Sanitizers ───────────────────────────────────────────────────────────────

/// Load a topic's atomic sanitizer registry: shared (`<config_root>/sanitizers.json`) merged with
/// the topic's own (`<topic>/sanitizers.json`), topic-local winning on name conflict.
/// Self-contained — a `Sanitizer` never references another named sanitizer — so nothing here needs
/// a resolution pass; only whatever references an entry by name (`sanitize:`) does. Each entry
/// accepts the same single-step-or-array sugar a `sanitize:` field does (`parse_sanitize_chain`) —
/// `HashMap<String, Vec<Sanitizer>>`'s own `Deserialize` can't apply that sugar itself (`Vec`'s
/// default `Deserialize` only accepts an array), so entries are read as raw `Value`s first.
pub fn load_topic_sanitizers(
    topic_dir: &std::path::Path,
    config_root: &std::path::Path,
) -> anyhow::Result<HashMap<String, Vec<Sanitizer>>> {
    let read = |path: &std::path::Path| -> anyhow::Result<HashMap<String, Vec<Sanitizer>>> {
        if !path.exists() {
            return Ok(HashMap::new());
        }
        let raw: HashMap<String, Value> = serde_json::from_str(&std::fs::read_to_string(path)?)
            .with_context(|| format!("parsing {}", path.display()))?;
        raw.into_iter()
            .map(|(name, v)| Ok((name, parse_sanitize_chain(v).with_context(|| format!("parsing {}", path.display()))?)))
            .collect()
    };
    let shared = read(&config_root.join("sanitizers.json"))?;
    let local = read(&topic_dir.join("sanitizers.json"))?;
    Ok(merge(&shared, &local))
}

// ── Shared producers ─────────────────────────────────────────────────────────

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
/// own producer JSON — any sibling keys the referencing site set (`from`/`annotate`) override the
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
