//! The atomic `&str -> atomic value` sanitize-chain machinery underneath an `Extract`'s
//! `sanitize:` field: `SanitizeRef` (a resolved-or-named chain reference, plus its load-time
//! `resolve` against the named sanitizer registry), `Sanitizer`/`Step` (the chain and its steps
//! — mapping/cases/filter/drop/replace/builtin), and the one built-in, `parse_length`.

use std::collections::HashMap;

use serde::{Deserialize, Deserializer};
use serde_json::Value;

/// The identity atomic transform: the value a bare tag read produces when no `sanitize` is named.
/// Not a special case — every `Extract` terminates in exactly one atomic transform; absent
/// `sanitize` just means "the identity one," same as any named entry in `sanitizers.json`.
fn identity(raw: &str) -> Value {
    Value::String(raw.to_owned())
}

/// An `Extract`/`Filter` `sanitize` reference. `Name` is what raw JSON always deserializes into
/// (tried first, so a plain string never lands in `Inline`); `resolve` (called once at load time,
/// alongside `Filter::expand`/`Producer::resolve`) replaces it with the actual chain, so `eval`
/// never does a registry lookup — same "resolve names once at load" treatment macros get.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum SanitizeRef {
    Name(String),
    Inline(Sanitizer),
}

impl SanitizeRef {
    fn eval(&self, raw: &str) -> Option<Value> {
        match self {
            SanitizeRef::Inline(chain) => chain.eval(raw),
            SanitizeRef::Name(name) => {
                tracing::error!("unresolved sanitizer '{name}' reached eval — resolve should have run at load");
                None
            }
        }
    }

    /// `name` not found in `sanitizers` falls back to a built-in alias (`Step::Builtin`, e.g.
    /// `"parse_length"`) rather than erroring — mirrors the pre-inlining fallback
    /// (`None => apply_builtin(name, raw)`). A truly unrecognized name still isn't caught until
    /// `apply_builtin` runs (it has no load-time name registry of its own, just one built-in), so
    /// it warns-and-drops per row rather than failing to load — the same looseness the built-in
    /// fallback always had.
    pub(crate) fn resolve(&self, sanitizers: &HashMap<String, Sanitizer>) -> anyhow::Result<SanitizeRef> {
        match self {
            SanitizeRef::Name(name) => Ok(SanitizeRef::Inline(match sanitizers.get(name) {
                Some(chain) => chain.clone(),
                None => Sanitizer(vec![Step::Builtin(name.clone())]),
            })),
            SanitizeRef::Inline(_) => Ok(self.clone()),
        }
    }
}

/// Evaluate a resolved `sanitize` reference against `raw`. `None` is the identity transform
/// (always succeeds) — see `SanitizeRef`.
pub fn resolve_sanitize(sanitize: Option<&SanitizeRef>, raw: &str) -> Option<Value> {
    match sanitize {
        None => Some(identity(raw)),
        Some(r) => r.eval(raw),
    }
}

/// An atomic `&str -> atomic` chain: an ordered list of `Step`s folded left (each step consumes
/// the previous string; the terminal step may yield any atomic `Value`). Always just a
/// `Vec<Step>` — see the hand-written `Deserialize` impl below for the JSON sugar (a bare single
/// step, with no wrapping array) that folds into a one-element `Vec` at parse time, so "a chain of
/// one" is never a distinct concept downstream (same treatment `Producer` gives its `fallback`
/// sugar).
#[derive(Debug, Clone)]
pub struct Sanitizer(Vec<Step>);

/// The JSON shapes a `Sanitizer` accepts: a single step (any of `Step`'s own shapes, including the
/// bare-string `Builtin` alias), or an explicit array of steps. Untagged, array tried first (a
/// step is never itself a JSON array).
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum SanitizerJson {
    Chain(Vec<Step>),
    One(Step),
}

impl<'de> Deserialize<'de> for Sanitizer {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Ok(match SanitizerJson::deserialize(deserializer)? {
            SanitizerJson::Chain(steps) => Sanitizer(steps),
            SanitizerJson::One(step) => Sanitizer(vec![step]),
        })
    }
}

impl Sanitizer {
    /// The chain's steps, in evaluation order — e.g. for diagnostics (`plot_dag`'s DAG rendering).
    pub fn steps(&self) -> &[Step] {
        &self.0
    }

    fn eval(&self, raw: &str) -> Option<Value> {
        let mut cur = Value::String(raw.to_owned());
        for s in &self.0 {
            cur = s.apply(cur.as_str()?)?;
        }
        Some(cur)
    }
}

// ── Chain steps: the atomic `&str -> atomic value` building blocks of a `Sanitizer` ──────

/// Accepts either `"foo"` or `["foo", "bar"]` in JSON.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum StrOrVec {
    One(String),
    Many(Vec<String>),
}

impl StrOrVec {
    pub(crate) fn into_vec(self) -> Vec<String> {
        match self {
            StrOrVec::One(s) => vec![s],
            StrOrVec::Many(v) => v,
        }
    }

    fn contains(&self, v: &str) -> bool {
        match self {
            StrOrVec::One(s) => s == v,
            StrOrVec::Many(vs) => vs.iter().any(|s| s == v),
        }
    }
}

/// One transform step: a lookup table or an allow-list.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum Step {
    /// Table lookup. Values may be any atomic JSON (string/bool/number) so a step can produce
    /// e.g. a boolean (`{ "yes": true }`). On a miss, `on_miss` decides: "keep" (passthrough),
    /// "drop"/absent (null), or any other string (a constant default).
    Mapping {
        mapping: HashMap<String, Value>,
        #[serde(default)]
        on_miss: Option<String>,
    },
    /// Inverted lookup shorthand: `{ "<output>": "<input>" | ["<input>", ...] }`. Collapses the
    /// common case of many inputs → one output.
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
    /// A built-in Rust transform as a (terminal) chain step, e.g. `"parse_length"`. Lets a data
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
    fn apply(&self, v: &str) -> Option<Value> {
        match self {
            Step::Mapping { mapping, on_miss } => match mapping.get(v) {
                Some(mapped) => Some(mapped.clone()),
                None => apply_on_miss(on_miss.as_deref(), v),
            },
            // Linear scan over the (typically short) case lists — no separate normalize-to-Mapping
            // pass needed; `cases` is authoring sugar, not a performance-sensitive hot path.
            Step::Cases { cases, on_miss } => {
                match cases.iter().find(|(_, inputs)| inputs.contains(v)) {
                    Some((output, _)) => Some(Value::String(output.clone())),
                    None => apply_on_miss(on_miss.as_deref(), v),
                }
            }
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
        }
    }
}

/// Shared `on_miss` handling for `Mapping`/`Cases`: "keep" (passthrough), "drop"/absent (null),
/// or any other string (a constant default).
fn apply_on_miss(on_miss: Option<&str>, v: &str) -> Option<Value> {
    match on_miss {
        Some("keep") => Some(Value::String(v.to_owned())),
        Some("drop") | None => None,
        Some(constant) => Some(Value::String(constant.to_owned())),
    }
}

// ── Built-in registry ───────────────────────────────────────────────────

/// Apply a named built-in `&str -> atomic` transform. Returns None when the value is rejected
/// (not in an allowed set / unparseable).
pub fn apply_builtin(name: &str, raw: &str) -> Option<Value> {
    match name {
        "parse_length" => parse_length(raw).map(|v| Value::Number(float_to_json(v))),
        // `parse_length` is the lone built-in: universal unit arithmetic, not a finite table.
        // Everything else (incl. the former `traffic_sign` country normalizer) lives in
        // sanitizers.json — as mapping/cases/filter/replace chains.
        other => { tracing::warn!("unknown built-in atomic transform: {other}"); None }
    }
}

fn float_to_json(v: f32) -> serde_json::Number {
    serde_json::Number::from_f64(v as f64).unwrap_or_else(|| serde_json::Number::from(0))
}

// ── parse_length ──────────────────────────────────────────────────────────────

/// Converts OSM length strings to metres. Handles: "2.5", "2.5 m", "250 cm", "2500 mm", "8 ft",
/// "8'6\"", … — the general `parse_compound_unit` algorithm over the `"length"` unit table
/// (`<config_root>/units.json`); no unit-specific logic lives here.
pub fn parse_length(raw: &str) -> Option<f32> {
    crate::units::parse_compound_unit(raw, crate::units::unit_table("length"))
}
