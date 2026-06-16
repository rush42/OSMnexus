//! Sanitizers: pure `&str -> atomic value` functions (the counterpart to `derive.rs`).
//! Each takes a single already-extracted tag value and returns a cleaned/validated atomic
//! output. Tag selection (which key, which side, obj/parent/centerline, fallbacks) is the
//! extraction layer's job (`engine/extract.rs`), not the sanitizer's.

use std::collections::HashMap;

use serde::Deserialize;
use serde_json::{Number, Value};

use crate::osm::types::RawTags;

// ── Key fallback primitive + sided lookups (used by the extraction layer + derive.rs) ─────────

/// The first-present fallback over an ordered list of candidate keys — the single primitive
/// behind both the `keys` extractor and the sided lookups. Returns the first key that is set.
pub(crate) fn first_present<'a, K: AsRef<str>>(
    tags: &'a RawTags,
    keys: impl IntoIterator<Item = K>,
) -> Option<&'a str> {
    keys.into_iter().find_map(|k| tags.get(k.as_ref()).map(String::as_str))
}

/// Candidate keys for a sided read, as a fallback list: `key:{side}` → `key:both`
/// → (left only, when `bare_left`) the bare `key`. A sided lookup is just `first_present`
/// over this list — what `getSided` / `getSidedWithBareLeft` were in Lua.
pub(crate) fn sided_keys(key: &str, side: &str, bare_left: bool) -> Vec<String> {
    let mut keys = vec![format!("{key}:{side}"), format!("{key}:both")];
    if bare_left && side == "left" {
        keys.push(key.to_owned());
    }
    keys
}

fn float_to_json(v: f32) -> Number {
    Number::from_f64(v as f64).unwrap_or_else(|| Number::from(0))
}

// ── Data-driven sanitizer chains (sanitizers.json) ────────────────────────────────
//
// A `SanitizerDef` is a pure `&str -> Option<atomic>` transform defined in data:
//   - an array of steps  → a chain (folded left, short-circuiting on drop)
//   - a single step object → `{ "mapping": {…}, "on_miss": … }` or `{ "filter": [...] }`
//   - a bare string        → an alias to a built-in Rust sanitizer
// Tag selection (which key, side, fallbacks) stays in the extraction layer; a sanitizer
// only ever sees one already-extracted value.

/// Accepts either `"foo"` or `["foo", "bar"]` in JSON.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum StrOrVec {
    One(String),
    Many(Vec<String>),
}

impl StrOrVec {
    fn into_vec(self) -> Vec<String> {
        match self {
            StrOrVec::One(s) => vec![s],
            StrOrVec::Many(v) => v,
        }
    }
}

/// One transform step: a lookup table or an allow-list.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum Step {
    /// Table lookup. Values may be any atomic JSON (string/bool/number) so a sanitizer can
    /// produce e.g. a boolean (`{ "yes": true }`). On a miss, `on_miss` decides: "keep"
    /// (passthrough), "drop"/absent (null), or any other string (a constant default).
    Mapping {
        mapping: HashMap<String, Value>,
        #[serde(default)]
        on_miss: Option<String>,
    },
    /// Inverted lookup shorthand: `{ "<output>": "<input>" | ["<input>", ...] }`. Collapses the
    /// common case of many inputs → one output. Normalized into a forward `Mapping` at load.
    Cases {
        cases: HashMap<String, StrOrVec>,
        #[serde(default)]
        on_miss: Option<String>,
    },
    /// Keep the value iff it is in the set, else drop (sugar for an identity mapping + drop).
    Filter { filter: Vec<String> },
    /// Drop the value iff it is in the set, else keep — the reject-list counterpart to `filter`.
    /// Dropping short-circuits the chain (e.g. `{ "drop": [""] }` to discard empty input).
    Drop { drop: Vec<String> },
    /// Literal string rewrites, applied in order (each transforms the running value, then the
    /// next sees the result — sed-like). A general, country-agnostic alternative to a hardcoded
    /// normalizer (e.g. the former `traffic_sign` builtin). Never drops.
    Replace { replace: Vec<ReplaceRule> },
    /// A built-in Rust sanitizer as a (terminal) chain step, e.g. `"parse_length"`. Lets a data
    /// chain end in an algorithmic, possibly non-string transform.
    Builtin(String),
}

/// One literal rewrite for a `replace` step.
#[derive(Debug, Clone, Deserialize)]
pub struct ReplaceRule {
    from: String,
    to: String,
    #[serde(default)]
    at: ReplaceAt,
}

/// Where a `ReplaceRule` matches: anywhere (replace every occurrence) or only as a prefix
/// (rewrite the leading `from`, keep the suffix; no-op when absent).
#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplaceAt {
    #[default]
    Anywhere,
    Prefix,
}

impl ReplaceRule {
    fn apply(&self, s: &str) -> String {
        match self.at {
            ReplaceAt::Anywhere => s.replace(&self.from, &self.to),
            ReplaceAt::Prefix => match s.strip_prefix(&self.from) {
                Some(rest) => format!("{}{rest}", self.to),
                None => s.to_owned(),
            },
        }
    }
}

impl Step {
    /// Expand a `Cases` step into the equivalent forward `Mapping`; other steps pass through.
    /// Done once at registry build so `apply` stays an O(1) lookup.
    fn normalize(self) -> Step {
        match self {
            Step::Cases { cases, on_miss } => {
                let mut mapping = HashMap::new();
                for (output, inputs) in cases {
                    for input in inputs.into_vec() {
                        let val = Value::String(output.clone());
                        if let Some(prev) = mapping.insert(input.clone(), val) {
                            tracing::warn!(
                                "sanitizer `cases`: input {input:?} maps to both {prev:?} and {output:?}"
                            );
                        }
                    }
                }
                Step::Mapping { mapping, on_miss }
            }
            other => other,
        }
    }

    fn apply(&self, v: &str) -> Option<Value> {
        match self {
            Step::Mapping { mapping, on_miss } => match mapping.get(v) {
                Some(mapped) => Some(mapped.clone()),
                None => match on_miss.as_deref() {
                    Some("keep") => Some(Value::String(v.to_owned())),
                    Some("drop") | None => None,
                    Some(constant) => Some(Value::String(constant.to_owned())),
                },
            },
            Step::Filter { filter } => {
                filter.iter().any(|a| a == v).then(|| Value::String(v.to_owned()))
            }
            Step::Drop { drop } => {
                (!drop.iter().any(|a| a == v)).then(|| Value::String(v.to_owned()))
            }
            Step::Replace { replace } => {
                let out = replace.iter().fold(v.to_owned(), |s, r| r.apply(&s));
                Some(Value::String(out))
            }
            Step::Builtin(name) => apply_builtin(name, v),
            // Normalized into Mapping at registry build (SanitizerRegistry::new).
            Step::Cases { .. } => unreachable!("`cases` step must be normalized before apply"),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum SanitizerDef {
    Chain(Vec<Step>),
    One(Step),
    /// Alias to a built-in Rust sanitizer (e.g. `"parse_length"`).
    Alias(String),
}

impl SanitizerDef {
    /// Normalize every `Cases` step into its forward `Mapping` (done once at registry build).
    fn normalize(self) -> Self {
        match self {
            SanitizerDef::One(step) => SanitizerDef::One(step.normalize()),
            SanitizerDef::Chain(steps) => {
                SanitizerDef::Chain(steps.into_iter().map(Step::normalize).collect())
            }
            alias => alias,
        }
    }

    /// Fold the steps. String-producing steps chain (each consumes the previous string); the
    /// final step may yield any atomic `Value`. `Alias` is resolved by the registry first.
    fn apply_chain(&self, raw: &str) -> Option<Value> {
        match self {
            SanitizerDef::One(step) => step.apply(raw),
            SanitizerDef::Chain(steps) => {
                let mut cur = Value::String(raw.to_owned());
                for s in steps {
                    cur = s.apply(cur.as_str()?)?;
                }
                Some(cur)
            }
            SanitizerDef::Alias(_) => None,
        }
    }
}

/// Resolves a sanitizer name to either a data-defined chain (sanitizers.json) or a built-in.
/// Custom definitions win; an unknown name falls back to the built-in registry.
#[derive(Debug, Default)]
pub struct SanitizerRegistry {
    custom: HashMap<String, SanitizerDef>,
}

impl SanitizerRegistry {
    pub fn new(custom: HashMap<String, SanitizerDef>) -> Self {
        let custom = custom.into_iter().map(|(k, v)| (k, v.normalize())).collect();
        Self { custom }
    }

    pub fn apply(&self, name: &str, raw: &str) -> Option<Value> {
        match self.custom.get(name) {
            Some(SanitizerDef::Alias(builtin)) => apply_builtin(builtin, raw),
            Some(def) => def.apply_chain(raw),
            None => apply_builtin(name, raw),
        }
    }
}

// ── Built-in sanitizer registry ───────────────────────────────────────────────────

/// Apply a named built-in `&str -> atomic` sanitizer. Returns None when the value is
/// rejected (not in an allowed set / unparseable).
pub fn apply_builtin(name: &str, raw: &str) -> Option<Value> {
    match name {
        "parse_length" => parse_length(raw).map(|v| Value::Number(float_to_json(v))),
        // `parse_length` is the lone built-in: universal unit arithmetic, not a finite table.
        // Everything else (incl. the former `traffic_sign` country normalizer) lives in
        // sanitizers.json — as mapping/cases/filter/replace chains.
        other => { tracing::warn!("unknown sanitizer: {other}"); None }
    }
}

// ── parse_length ──────────────────────────────────────────────────────────────

/// Port of parse_length.lua. Converts OSM length strings to metres as f32.
/// Handles: "2.5", "2.5 m", "250 cm", "2500 mm", "8 ft", "8'6\"", …
pub fn parse_length(raw: &str) -> Option<f32> {
    let s = raw.trim();
    if s.is_empty() { return None; }

    // feet/inches: 8'6" or 8'
    if s.contains('\'') {
        let parts: Vec<&str> = s.split('\'').collect();
        let feet: f32 = parts[0].trim().parse().ok()?;
        let inches: f32 = parts.get(1)
            .map(|p| p.trim().trim_end_matches('"'))
            .and_then(|p| if p.is_empty() { Some("0") } else { Some(p) })
            .and_then(|p| p.parse().ok())
            .unwrap_or(0.0);
        return Some((feet * 12.0 + inches) * 0.0254);
    }

    // strip unit suffix and scale
    let (num_str, scale) = if let Some(n) = s.strip_suffix("km") {
        (n.trim(), 1000.0_f32)
    } else if let Some(n) = s.strip_suffix("cm") {
        (n.trim(), 0.01_f32)
    } else if let Some(n) = s.strip_suffix("mm") {
        (n.trim(), 0.001_f32)
    } else if let Some(n) = s.strip_suffix("ft") {
        (n.trim(), 0.3048_f32)
    } else if let Some(n) = s.strip_suffix(" m") {
        (n.trim(), 1.0_f32)
    } else if let Some(n) = s.strip_suffix('m') {
        (n.trim(), 1.0_f32)
    } else {
        (s, 1.0_f32)
    };

    let v: f32 = num_str.replace(',', ".").parse().ok()?;
    Some(v * scale)
}


