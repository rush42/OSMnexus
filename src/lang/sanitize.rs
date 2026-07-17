//! The atomic `&str -> atomic value` sanitize-chain machinery underneath an `Extract`'s
//! `sanitize:` field: `Sanitizer`/`Step` (the chain and its steps — really just
//! mapping/replace/builtin) and the one built-in, `parse_length`. A named `sanitize: "<name>"`
//! reference is never a distinct type here — `topic::load::inline_sanitize_refs` splices the
//! resolved chain's own JSON (via `Sanitizer`/`Step`'s `Serialize` impls below) into place at
//! load time, before any `Filter`/`Producer` JSON is deserialized, so `sanitize:` always
//! deserializes straight into `Option<Sanitizer>` (see `resolve_named_sanitizer`, the one piece of
//! by-name lookup logic, shared by that inlining pass and `topic::spec::resolve_output_entry`'s
//! own by-name shorthand). Neither `Sanitizer` nor `Step` derives `Deserialize`/`Serialize` here —
//! their JSON sugar (a bare single step instead of an array; `cases`/`filter`/`drop` as sugar for
//! `mapping`, only on the way in) is folded in by hand-written impls in `parser`, kept separate
//! from the runtime types/eval logic defined here.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The identity atomic transform: the value a bare tag read produces when no `sanitize` is named.
/// Not a special case — every `Extract` terminates in exactly one atomic transform; absent
/// `sanitize` just means "the identity one," same as any named entry in `sanitizers.json`.
fn identity(raw: &str) -> Value {
    Value::String(raw.to_owned())
}

/// `name` not found in `sanitizers` falls back to a built-in alias (`Step::Builtin`, e.g.
/// `"parse_length"`) rather than erroring — mirrors the pre-inlining fallback
/// (`None => apply_builtin(name, raw)`). A truly unrecognized name still isn't caught until
/// `apply_builtin` runs (it has no load-time name registry of its own, just one built-in), so it
/// warns-and-drops per row rather than failing to load — the same looseness the built-in fallback
/// always had. The one piece of by-name sanitizer resolution logic, shared by
/// `topic::load::inline_sanitize_refs` (JSON-level `sanitize: "name"`) and
/// `topic::spec::resolve_output_entry` (the sanitizer-shorthand `{ "name": ... }` output entry,
/// which never goes through JSON inlining since it isn't spelled as a `sanitize:` field).
pub fn resolve_named_sanitizer(name: &str, sanitizers: &HashMap<String, Sanitizer>) -> Sanitizer {
    match sanitizers.get(name) {
        Some(chain) => chain.clone(),
        None => Sanitizer::from_steps(vec![Step::Builtin(name.to_owned())]),
    }
}

/// Evaluate a resolved `sanitize` chain against `raw`. `None` is the identity transform
/// (always succeeds) — every `sanitize:` field is `Option<Sanitizer>`, already resolved by the
/// time `Filter`/`Producer` deserialize it (see `resolve_named_sanitizer`'s own doc).
pub fn eval_sanitize(sanitize: Option<&Sanitizer>, raw: &str) -> Option<Value> {
    match sanitize {
        None => Some(identity(raw)),
        Some(chain) => chain.eval(raw),
    }
}

/// An atomic `&str -> atomic` chain: an ordered list of `Step`s folded left (each step consumes
/// the previous string; the terminal step may yield any atomic `Value`). Always just a
/// `Vec<Step>` — `parser`'s hand-written `Deserialize` impl folds the JSON sugar (a
/// bare single step, with no wrapping array) into a one-element `Vec` at parse time, so "a chain
/// of one" is never a distinct concept downstream (same treatment `Producer` gives its `fallback`
/// sugar). The field is private; `parser` builds a `Sanitizer` through `from_steps` rather than
/// reaching into it directly.
#[derive(Debug, Clone)]
pub struct Sanitizer(Vec<Step>);

/// Serializes as the canonical array-of-steps shape — `parser`'s `Deserialize` impl accepts that
/// shape back (`SanitizerJson::Chain`) regardless of chain length, so this round-trips even a
/// one-step chain. The one consumer: `topic::load::inline_sanitize_refs`, splicing a resolved
/// `sanitize: "<name>"` reference back into the raw JSON `Value` tree before `Filter`/`Producer`
/// ever deserialize it.
impl serde::Serialize for Sanitizer {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(serializer)
    }
}

impl Sanitizer {
    /// Construct directly from already-known steps — used by `parser`'s `Deserialize`
    /// impl (after folding any JSON sugar) and by `resolve_named_sanitizer`'s builtin-name fallback.
    /// Built by folding each step onto the chain one at a time: append it, unless it composes with
    /// the current last step (`Step::compose`) — e.g. `cases` normalizing raw values then `filter`
    /// keeping only an allowed set, both desugar to `Step::Mapping` (see `parser`) — in which case
    /// the composed step replaces the last one instead. Folding one step at a time (rather than
    /// scanning the whole `Vec` for runs) is what makes a run of N collapse correctly: each new
    /// step composes against whatever the previous compose already produced, however long the run.
    pub(crate) fn from_steps(steps: Vec<Step>) -> Self {
        Sanitizer(steps.into_iter().fold(Vec::new(), |mut chain, step| {
            match chain.last().and_then(|last| last.compose(&step)) {
                Some(composed) => *chain.last_mut().unwrap() = composed,
                None => chain.push(step),
            }
            chain
        }))
    }

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
}

/// One transform step: a table lookup (`Mapping`), literal rewrites (`Replace`), or a built-in
/// (`Builtin`) — see `parser`'s hand-written `Deserialize` impl for the JSON sugar
/// (`cases`/`filter`/`drop`) that folds into `Mapping` at parse time, so a `Step` value is only
/// ever one of these three:
/// - `cases` (`{ "<output>": "<input>" | ["<input>", ...] }`, the inverted-lookup shorthand)
///   expands to one `mapping` entry per input, all pointing at the same output.
/// - `filter` (keep iff a member, else drop) becomes an identity `mapping` (each listed value maps
///   to itself) with no `on_miss` — Mapping's own default (absent `on_miss` drops) does the rest.
/// - `drop` (drop iff a member, else keep unchanged) becomes a `mapping` where each listed value
///   maps to JSON `null` — the sentinel meaning "found, but drop anyway" (see `Mapping`'s own doc)
///   — with `on_miss: "keep"` so anything else passes through.
#[derive(Debug, Clone)]
pub enum Step {
    /// Table lookup. A mapped value is any atomic JSON (string/bool/number) so a step can produce
    /// e.g. a boolean (`{ "yes": true }`) — except JSON `null`, reserved as the "found this key,
    /// but drop the value anyway" sentinel (distinct from `on_miss`'s own drop-on-absence; see
    /// `Step`'s own doc for why `drop` needs it). On a miss, `on_miss` decides: "keep"
    /// (passthrough), "drop"/absent (null), or any other string (a constant default).
    Mapping {
        mapping: HashMap<String, Value>,
        on_miss: Option<String>,
    },
    /// Literal string rewrites, applied in order (each transforms the running value, then the
    /// next sees the result — sed-like). A general, country-agnostic alternative to a hardcoded
    /// normalizer (e.g. the former `traffic_sign` builtin). Never drops.
    Replace { replace: Vec<ReplaceRule> },
    /// A built-in Rust transform as a (terminal) chain step, e.g. `"parse_length"`. Lets a data
    /// chain end in an algorithmic, possibly non-string transform.
    Builtin(String),
}

/// The `Serialize` counterpart to `parser`'s hand-written `Step` `Deserialize` — canonical shapes
/// only (`Mapping`/`Replace` as their own object, `Builtin` as a bare string), never the folded
/// `cases`/`filter`/`drop` sugar (that sugar only ever exists transiently on the way in). Same one
/// consumer as `Sanitizer`'s own `Serialize`.
impl serde::Serialize for Step {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        match self {
            Step::Mapping { mapping, on_miss } => {
                let mut m = serializer.serialize_map(Some(if on_miss.is_some() { 2 } else { 1 }))?;
                m.serialize_entry("mapping", mapping)?;
                if let Some(on_miss) = on_miss {
                    m.serialize_entry("on_miss", on_miss)?;
                }
                m.end()
            }
            Step::Replace { replace } => {
                let mut m = serializer.serialize_map(Some(1))?;
                m.serialize_entry("replace", replace)?;
                m.end()
            }
            Step::Builtin(name) => serializer.serialize_str(name),
        }
    }
}

/// One literal rewrite for a `replace` step.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ReplaceRule {
    from: String,
    to: String,
    #[serde(default)]
    at: ReplaceAt,
}

/// Where a `ReplaceRule` matches: anywhere (replace every occurrence) or only as a prefix
/// (rewrite the leading `from`, keep the suffix; no-op when absent).
#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize)]
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
                Some(Value::Null) => None, // found, but marked "drop" (see `Step`'s own doc)
                Some(mapped) => Some(mapped.clone()),
                None => apply_on_miss(on_miss.as_deref(), v),
            },
            Step::Replace { replace } => {
                let out = replace.iter().fold(v.to_owned(), |s, r| r.apply(&s));
                Some(Value::String(out))
            }
            Step::Builtin(name) => apply_builtin(name, v),
        }
    }

    /// If `self` immediately followed by `next` can be represented as one step running in their
    /// place — today only two `Mapping`s, via `compose_mappings` — return it; `None` when they
    /// can't be merged (different variants, or a `Mapping` pair whose merge isn't representable),
    /// meaning `next` stays a separate step. The one place adjacent-step folding is decided;
    /// `Sanitizer::from_steps` is the one caller, applying it once per incoming step.
    fn compose(&self, next: &Step) -> Option<Step> {
        match (self, next) {
            (
                Step::Mapping { mapping: map_a, on_miss: miss_a },
                Step::Mapping { mapping: map_b, on_miss: miss_b },
            ) => {
                let (mapping, on_miss) =
                    compose_mappings(map_a, miss_a.as_deref(), map_b, miss_b.as_deref())?;
                Some(Step::Mapping { mapping, on_miss })
            }
            _ => None,
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

// ── Adjacent-Mapping composition ────────────────────────────────────────────────

/// Compose `Mapping(map_a, miss_a)` then `Mapping(map_b, miss_b)` — matching `Sanitizer::eval`'s
/// own step-chaining exactly, including its `cur.as_str()?` gate between steps (a non-string A
/// output, e.g. a `mapping` producing a bool, aborts the chain right there rather than reaching
/// B) — into one `(mapping, on_miss)` pair, or `None` when the result can't be represented as a
/// single `Mapping` (its `on_miss` would have to encode a non-string constant, since `Step::Mapping`
/// only supports "keep"/"drop"/a string default).
fn compose_mappings(
    map_a: &HashMap<String, Value>,
    miss_a: Option<&str>,
    map_b: &HashMap<String, Value>,
    miss_b: Option<&str>,
) -> Option<(HashMap<String, Value>, Option<String>)> {
    // What every key *not* explicitly enumerated below resolves to. Only representable when A's
    // own on_miss is "keep" (falls straight through to B's on_miss unchanged) or gives a fixed
    // string/drop result for every such key (A's on_miss is itself "drop", or a string constant
    // whose own lookup through B is a string or a drop) — anything else (a non-string A on_miss
    // result feeding into B) has no single `on_miss` that can express it.
    let on_miss = match miss_a {
        None | Some("drop") => None,
        Some("keep") => miss_b.map(str::to_owned),
        Some(constant) => match map_b.get(constant) {
            Some(Value::Null) => None,
            Some(Value::String(s)) => Some(s.clone()),
            Some(_) => return None,
            None => match miss_b {
                None | Some("drop") => None,
                Some("keep") => Some(constant.to_owned()),
                Some(c2) => Some(c2.to_owned()),
            },
        },
    };

    // Every key either table might branch on needs its own composed entry: A's own keys always
    // (they're never covered by A's on_miss), plus B's keys too when an A-miss falls through to B
    // unchanged (on_miss "keep") — otherwise a key present only in B is unreachable through A and
    // irrelevant here.
    let mut keys: std::collections::HashSet<&str> = map_a.keys().map(String::as_str).collect();
    if miss_a == Some("keep") {
        keys.extend(map_b.keys().map(String::as_str));
    }

    let mut mapping = HashMap::with_capacity(keys.len());
    for k in keys {
        let a_out = match map_a.get(k) {
            Some(v) => v.clone(),
            None => match apply_on_miss(miss_a, k) {
                Some(v) => v,
                None => { mapping.insert(k.to_owned(), Value::Null); continue; }
            },
        };
        let composed = match a_out.as_str() {
            None => None, // non-string A output: the next step's `cur.as_str()?` would abort here too
            Some(s) => match map_b.get(s) {
                Some(v) => Some(v.clone()),
                None => apply_on_miss(miss_b, s),
            },
        };
        mapping.insert(k.to_owned(), composed.unwrap_or(Value::Null));
    }

    Some((mapping, on_miss))
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

#[cfg(test)]
mod compose_tests {
    use super::*;

    fn sanitizer(json: &str) -> Sanitizer {
        serde_json::from_str(json).unwrap()
    }

    /// `bikelanes/sanitizers.json`'s `traffic_mode`: `cases` (on_miss keep) then `filter`
    /// (on_miss drop) — the real two-step chain that motivated composing adjacent `Mapping`s.
    #[test]
    fn traffic_mode_collapses_to_one_step_and_matches_sequential_semantics() {
        let s = sanitizer(r#"[
            { "cases": { "foot": "foot;bicycle", "motor_vehicle": "motorized", "no": "none" }, "on_miss": "keep" },
            { "filter": ["no", "motor_vehicle", "parking", "psv", "bicycle", "foot"] }
        ]"#);
        assert_eq!(s.steps().len(), 1, "adjacent Mapping steps should collapse into one");

        // `cases` is an inverted lookup (`{output: input}`): "foot;bicycle" renames to "foot", and
        // the renamed value survives `filter`'s allow-list.
        assert_eq!(eval_sanitize(Some(&s), "foot;bicycle"), Some(Value::String("foot".to_owned())));
        assert_eq!(eval_sanitize(Some(&s), "motorized"), Some(Value::String("motor_vehicle".to_owned())));
        assert_eq!(eval_sanitize(Some(&s), "none"), Some(Value::String("no".to_owned())));
        // Untouched by `cases` (on_miss keep, not a listed input) but present in `filter`'s
        // allow-list as-is.
        assert_eq!(eval_sanitize(Some(&s), "bicycle"), Some(Value::String("bicycle".to_owned())));
        // Untouched by `cases`, and not in `filter`'s allow-list: dropped.
        assert_eq!(eval_sanitize(Some(&s), "unknown_value"), None);
    }

    /// `bikelanes/sanitizers.json`'s `separation` chain — same shape, larger tables.
    #[test]
    fn separation_collapses_to_one_step_and_matches_sequential_semantics() {
        let s = sanitizer(r#"[
            { "cases": {
                "bump": ["separation_kerb", "lane_separator"],
                "no": ["surface"],
                "kerb": ["kerb;greenery"]
              }, "on_miss": "keep" },
            { "filter": ["no", "bump", "kerb", "yes"] }
        ]"#);
        assert_eq!(s.steps().len(), 1);

        assert_eq!(eval_sanitize(Some(&s), "separation_kerb"), Some(Value::String("bump".to_owned())));
        assert_eq!(eval_sanitize(Some(&s), "surface"), Some(Value::String("no".to_owned())));
        assert_eq!(eval_sanitize(Some(&s), "yes"), Some(Value::String("yes".to_owned()))); // passthrough, in filter's allow-list
        assert_eq!(eval_sanitize(Some(&s), "solid_line"), None); // passthrough, not in allow-list: dropped
    }

    /// A mapping whose miss is a constant that itself gets rewritten by the next mapping —
    /// exercises the `Some(constant)` branch of `compose_on_miss`.
    #[test]
    fn constant_on_miss_resolved_through_next_mapping() {
        let s = sanitizer(r#"[
            { "mapping": { "a": "x" }, "on_miss": "fallback" },
            { "mapping": { "x": "X", "fallback": "FALLBACK" } }
        ]"#);
        assert_eq!(s.steps().len(), 1);
        assert_eq!(eval_sanitize(Some(&s), "a"), Some(Value::String("X".to_owned())));
        assert_eq!(eval_sanitize(Some(&s), "anything_else"), Some(Value::String("FALLBACK".to_owned())));
    }

    /// A mapping producing a non-string value (e.g. a bool) mid-chain aborts, same as
    /// `Sanitizer::eval`'s own `cur.as_str()?` gate between steps — composition must preserve that.
    #[test]
    fn non_string_intermediate_output_still_aborts_the_chain() {
        let s = sanitizer(r#"[
            { "mapping": { "yes": true }, "on_miss": "keep" },
            { "mapping": { "yes": "irrelevant" } }
        ]"#);
        assert_eq!(s.steps().len(), 1);
        assert_eq!(eval_sanitize(Some(&s), "yes"), None);
    }

    /// Differential check: for a handful of representative `(mapping, on_miss)` pairs, the
    /// composed single step must agree with manually chaining the two original steps (mirroring
    /// `Sanitizer::eval`'s own `s.apply(cur.as_str()?)` loop) for every candidate input, including
    /// inputs neither table mentions.
    #[test]
    fn composed_matches_manual_two_step_chaining() {
        fn check(a: Step, b: Step) {
            let candidates = [
                "a", "b", "c", "x", "y", "z", "1", "2", "", "foo", "bar", "unmapped_anywhere",
            ];
            let composed = Sanitizer::from_steps(vec![a.clone(), b.clone()]);
            assert_eq!(composed.steps().len(), 1, "expected these two Mapping steps to compose");
            for input in candidates {
                let manual = (|| -> Option<Value> {
                    let mid = a.apply(input)?;
                    b.apply(mid.as_str()?)
                })();
                let via_composed = eval_sanitize(Some(&composed), input);
                assert_eq!(manual, via_composed, "mismatch for input {input:?}");
            }
        }

        let mapping = |pairs: &[(&str, Value)], on_miss: Option<&str>| Step::Mapping {
            mapping: pairs.iter().map(|(k, v)| (k.to_string(), v.clone())).collect(),
            on_miss: on_miss.map(str::to_owned),
        };

        // on_miss keep -> on_miss drop (the real traffic_mode/separation shape).
        check(
            mapping(&[("a", Value::String("x".into())), ("b", Value::String("y".into()))], Some("keep")),
            mapping(&[("x", Value::String("X".into()))], None),
        );
        // on_miss drop -> on_miss keep.
        check(
            mapping(&[("a", Value::String("x".into()))], None),
            mapping(&[("x", Value::String("X".into()))], Some("keep")),
        );
        // on_miss constant -> on_miss constant, constant resolves through B to another constant.
        check(
            mapping(&[("a", Value::String("x".into()))], Some("fallback")),
            mapping(&[("fallback", Value::String("F".into())), ("x", Value::String("X".into()))], Some("other")),
        );
        // Explicit `null` (drop sentinel) entries in both tables.
        check(
            mapping(&[("a", Value::Null), ("b", Value::String("x".into()))], Some("keep")),
            mapping(&[("x", Value::Null), ("b", Value::String("kept_b".into()))], None),
        );
        // Non-string A output (mid-chain abort) mixed with string outputs.
        check(
            mapping(&[("a", Value::Bool(true)), ("b", Value::String("x".into()))], Some("keep")),
            mapping(&[("x", Value::String("X".into()))], Some("keep")),
        );
    }

    /// Three-in-a-row: composing greedily left-to-right should collapse a run of any length, not
    /// just pairs.
    #[test]
    fn three_consecutive_mappings_collapse_to_one() {
        let s = sanitizer(r#"[
            { "mapping": { "a": "b" } },
            { "mapping": { "b": "c" } },
            { "mapping": { "c": "d" } }
        ]"#);
        assert_eq!(s.steps().len(), 1);
        assert_eq!(eval_sanitize(Some(&s), "a"), Some(Value::String("d".to_owned())));
    }
}
