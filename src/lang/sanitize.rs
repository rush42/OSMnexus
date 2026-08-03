//! The atomic `&str -> atomic value` sanitize-chain machinery underneath an `Extract`'s
//! `sanitize:` field: `Sanitizer` (one step — a table lookup, literal rewrites, or a built-in) and
//! the chain it's folded over, a plain `Vec<Sanitizer>` (an empty chain is the identity transform —
//! no separate wrapper type needed) plus the one built-in, `parse_length`. A named
//! `sanitize: "<name>"` reference is never a distinct type here — `topic::load::inline_sanitize_refs`
//! splices the resolved chain's own JSON (via `Sanitizer`'s `Serialize` impl below) into place at
//! load time, before any `Filter`/`Producer` JSON is deserialized, so `sanitize:` always
//! deserializes straight into `Vec<Sanitizer>` (see `resolve_named_sanitizer`, the one piece of
//! by-name lookup logic, shared by that inlining pass and `topic::spec::resolve_output_entry`'s
//! own by-name shorthand). `Sanitizer` doesn't derive `Deserialize` here — its JSON sugar (a bare
//! single step instead of an array; `cases`/`filter`/`drop` as sugar for `mapping`, only on the way
//! in) is folded in by hand-written impls in `parser`, kept separate from the runtime types/eval
//! logic defined here.

use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap};

use anyhow::Context;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The identity atomic transform: the value a bare tag read produces when no `sanitize` is named.
/// Not a special case — every `Extract` terminates in exactly one atomic transform; absent
/// `sanitize` just means "the identity one," same as any named entry in `sanitizers.json`.
fn identity(raw: &str) -> Value {
    Value::String(raw.to_owned())
}

/// `name` not found in `sanitizers` falls back to a built-in alias (`Sanitizer::Builtin`, e.g.
/// `"parse_length"`) rather than erroring; a name that isn't a named sanitizer *or* a known
/// built-in fails loudly here, at load time (`Builtin` is a closed enum, not a passthrough
/// `String`). The one piece of by-name sanitizer resolution logic, shared by
/// `topic::load::inline_sanitize_refs` (JSON-level `sanitize: "name"`) and
/// `topic::spec::resolve_output_entry` (the sanitizer-shorthand `{ "sanitizer": ... }` output entry,
/// which never goes through JSON inlining since it isn't spelled as a `sanitize:` field).
pub fn resolve_named_sanitizer(name: &str, sanitizers: &HashMap<String, Vec<Sanitizer>>) -> anyhow::Result<Vec<Sanitizer>> {
    match sanitizers.get(name) {
        Some(chain) => Ok(chain.clone()),
        None => Builtin::from_name(name)
            .map(|b| vec![Sanitizer::Builtin(b)])
            .with_context(|| format!("`{name}` is not a named sanitizer or a built-in")),
    }
}

/// Evaluate a resolved `sanitize` chain against `raw`, folded left (each step consumes the
/// previous string; the terminal step may yield any atomic `Value`). An empty chain is the
/// identity transform (always succeeds) — every `sanitize:` field is a plain `Vec<Sanitizer>`,
/// already resolved by the time `Filter`/`Producer` deserialize it (see `resolve_named_sanitizer`'s
/// own doc).
pub fn eval_sanitize(chain: &[Sanitizer], raw: &str) -> Option<Value> {
    // First step reads `raw` directly (no upfront `to_owned` just to hand a `&str` right back
    // out) — only a step past the first needs a materialized `Value` to read `.as_str()` from.
    let mut steps = chain.iter();
    let mut cur = match steps.next() {
        Some(first) => first.apply(raw)?,
        None => return Some(identity(raw)),
    };
    for s in steps {
        cur = s.apply(cur.as_str()?)?;
    }
    Some(cur)
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
}

/// A `Sanitizer::Mapping` entry's value, restricted to what a mapping step ever actually produces —
/// string/bool/number, never a nested array/object. Exists (instead of the raw `serde_json::Value`
/// the JSON side still uses) purely so `Sanitizer`/`Vec<Sanitizer>` can derive `Eq`/`Hash`/`Ord`: `Value` isn't
/// `Eq`/`Hash` (its `Number` can hold a `NaN`-capable `f64`), the same problem
/// `Predicate::Num` already solves by bit-casting its threshold to `u64` — same trick here.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum AtomicJson {
    Str(String),
    Bool(bool),
    /// An `f64`'s raw bit pattern (`to_bits`/`from_bits`) — see this type's own doc.
    Num(u64),
}

impl AtomicJson {
    /// `None` for anything not atomic (array/object) — a `Sanitizer::Mapping` entry may only ever be
    /// string/bool/number (see `Sanitizer::Mapping`'s own doc); `Value::Null` is handled by the caller
    /// (it's the drop sentinel, not a value `AtomicJson` represents). `pub(crate)`: `parser`'s
    /// hand-written `Sanitizer` `Deserialize` impl needs it to convert the JSON-side raw `Value` map.
    pub(crate) fn from_value(v: &Value) -> Option<Self> {
        match v {
            Value::String(s) => Some(AtomicJson::Str(s.clone())),
            Value::Bool(b) => Some(AtomicJson::Bool(*b)),
            Value::Number(n) => Some(AtomicJson::Num(n.as_f64()?.to_bits())),
            Value::Null | Value::Array(_) | Value::Object(_) => None,
        }
    }

    pub(crate) fn into_value(self) -> Value {
        match self {
            AtomicJson::Str(s) => Value::String(s),
            AtomicJson::Bool(b) => Value::Bool(b),
            AtomicJson::Num(bits) => Value::Number(
                serde_json::Number::from_f64(f64::from_bits(bits)).unwrap_or_else(|| serde_json::Number::from(0)),
            ),
        }
    }
}

/// One sanitize-chain step: a table lookup (`Mapping`), literal rewrites (`Replace`), or a built-in
/// (`Builtin`) — see `parser`'s hand-written `Deserialize` impl for the JSON sugar
/// (`cases`/`filter`/`drop`) that folds into `Mapping` at parse time, so a `Sanitizer` value is only
/// ever one of these three:
/// - `cases` (`{ "<output>": "<input>" | ["<input>", ...] }`, the inverted-lookup shorthand)
///   expands to one `mapping` entry per input, all pointing at the same output.
/// - `filter` (keep iff a member, else drop) becomes an identity `mapping` (each listed value maps
///   to itself) with no `on_miss` — Mapping's own default (absent `on_miss` drops) does the rest.
/// - `drop` (drop iff a member, else keep unchanged) becomes a `mapping` where each listed value
///   maps to JSON `null` — the sentinel meaning "found, but drop anyway" (see `Mapping`'s own doc)
///   — with `on_miss: "keep"` so anything else passes through.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Sanitizer {
    /// Table lookup. A mapped value is any atomic JSON (string/bool/number) so a step can produce
    /// e.g. a boolean (`{ "yes": true }`) — except a missing (`None`) entry value, reserved as the
    /// "found this key, but drop the value anyway" sentinel (distinct from `on_miss`'s own
    /// drop-on-absence; see `Sanitizer`'s own doc for why `drop` needs it). On a miss, `on_miss`
    /// decides: "keep" (passthrough), "drop"/absent (null), or any other string (a constant
    /// default). `BTreeMap` (not `HashMap`) so the whole chain stays `Hash`/`Ord`.
    Mapping {
        mapping: BTreeMap<String, Option<AtomicJson>>,
        on_miss: Option<String>,
    },
    /// Literal string rewrites, applied in order (each transforms the running value, then the
    /// next sees the result — sed-like). A general, country-agnostic alternative to a hardcoded
    /// normalizer (e.g. the former `traffic_sign` builtin). Never drops.
    Replace { replace: Vec<ReplaceRule> },
    /// A built-in Rust transform as a (terminal) chain step. Lets a data chain end in an
    /// algorithmic, possibly non-string transform.
    Builtin(Builtin),
}

/// The registry of built-in Rust sanitize transforms — every name a `sanitize:`/`Builtin` JSON
/// string can resolve to. A closed enum (not a `String`) so an unrecognized name is a load-time
/// error (`resolve_named_sanitizer`, `parser`'s `Sanitizer` `Deserialize` impl) rather than a
/// per-row runtime warn-and-drop.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Builtin {
    ParseLength,
}

impl Builtin {
    pub(crate) fn from_name(name: &str) -> Option<Self> {
        match name {
            "parse_length" => Some(Builtin::ParseLength),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Builtin::ParseLength => "parse_length",
        }
    }

    fn apply(self, raw: &str) -> Option<Value> {
        match self {
            // `parse_length` is the lone built-in: universal unit arithmetic, not a finite table.
            // Everything else (incl. the former `traffic_sign` country normalizer) lives in
            // sanitizers.json — as mapping/cases/filter/replace chains.
            Builtin::ParseLength => parse_length(raw).map(|v| Value::Number(float_to_json(v))),
        }
    }
}

/// The `Serialize` counterpart to `parser`'s hand-written `Sanitizer` `Deserialize` — canonical shapes
/// only (`Mapping`/`Replace` as their own object, `Builtin` as a bare string), never the folded
/// `cases`/`filter`/`drop` sugar (that sugar only ever exists transiently on the way in). Same one
/// consumer as `Sanitizer`'s own `Serialize`.
impl serde::Serialize for Sanitizer {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        match self {
            Sanitizer::Mapping { mapping, on_miss } => {
                let mut m = serializer.serialize_map(Some(if on_miss.is_some() { 2 } else { 1 }))?;
                let mapping: std::collections::BTreeMap<&str, Value> = mapping
                    .iter()
                    .map(|(k, v)| (k.as_str(), v.clone().map(AtomicJson::into_value).unwrap_or(Value::Null)))
                    .collect();
                m.serialize_entry("mapping", &mapping)?;
                if let Some(on_miss) = on_miss {
                    m.serialize_entry("on_miss", on_miss)?;
                }
                m.end()
            }
            Sanitizer::Replace { replace } => {
                let mut m = serializer.serialize_map(Some(1))?;
                m.serialize_entry("replace", replace)?;
                m.end()
            }
            Sanitizer::Builtin(b) => serializer.serialize_str(b.name()),
        }
    }
}

/// One literal rewrite for a `replace` step.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Deserialize, Serialize)]
pub struct ReplaceRule {
    pub(crate) from: String,
    pub(crate) to: String,
    #[serde(default)]
    pub(crate) at: ReplaceAt,
}

/// Where a `ReplaceRule` matches: anywhere (replace every occurrence) or only as a prefix
/// (rewrite the leading `from`, keep the suffix; no-op when absent).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplaceAt {
    #[default]
    Anywhere,
    Prefix,
}

impl ReplaceRule {
    /// Borrows `s` back unchanged when this rule doesn't match — only a rule that actually fires
    /// allocates, so a chain of mostly-inapplicable rules (the common case: a `replace` list
    /// usually targets one or two specific spellings) costs one allocation total, not one per rule.
    fn apply<'a>(&self, s: Cow<'a, str>) -> Cow<'a, str> {
        match self.at {
            ReplaceAt::Anywhere => {
                if s.contains(&self.from) { Cow::Owned(s.replace(&self.from, &self.to)) } else { s }
            }
            ReplaceAt::Prefix => match s.strip_prefix(&self.from) {
                Some(rest) => Cow::Owned(format!("{}{rest}", self.to)),
                None => s,
            },
        }
    }
}

impl Sanitizer {
    fn apply(&self, v: &str) -> Option<Value> {
        match self {
            Sanitizer::Mapping { mapping, on_miss } => match mapping.get(v) {
                Some(None) => None, // found, but marked "drop" (see `Sanitizer`'s own doc)
                Some(Some(mapped)) => Some(mapped.clone().into_value()),
                None => apply_on_miss(on_miss.as_deref(), v),
            },
            Sanitizer::Replace { replace } => {
                let out = replace.iter().fold(Cow::Borrowed(v), |s, r| r.apply(s));
                Some(Value::String(out.into_owned()))
            }
            Sanitizer::Builtin(b) => b.apply(v),
        }
    }
}

/// Shared `on_miss` handling for `Mapping`: "keep" (passthrough), "drop"/absent (null), or any
/// other string (a constant default).
fn apply_on_miss(on_miss: Option<&str>, v: &str) -> Option<Value> {
    match on_miss {
        Some("keep") => Some(Value::String(v.to_owned())),
        Some("drop") | None => None,
        Some(constant) => Some(Value::String(constant.to_owned())),
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
