//! The JSON schema types a topic's `topic.json` (plus `sanitizers`/`derivers` desugaring)
//! deserializes into. Pure load-time data model — no per-object evaluation lives here.

use serde::Deserialize;
use serde_json::Value;
use crate::tag_engine::categories::DeriverBinding;
use crate::tag_engine::filter::Filter;
use crate::tag_engine::producer::{Producer, TagSet};
use crate::tag_engine::sanitize::{SanitizeRef, StrOrVec};

#[derive(Debug, Deserialize)]
pub struct TopicSpec {
    pub table: String,
    /// Center-line splits — the engine's one built-in cardinality-changing transform (unnests a
    /// side's tags onto its own object; see `SplitSidesSpec`). Always applied last, after every
    /// `input_transforms` entry, regardless of where it'd fall in declaration order — so it lives
    /// in its own top-level list rather than interleaved into `input_transforms`.
    #[serde(default)]
    pub split_sides: Vec<SplitSidesSpec>,
    /// Ordered pipeline of in-place tag rewrites, applied before categorization (see
    /// `tag_engine::input_transforms::InputTransform`). Each entry is data-driven — no `transform`
    /// discriminator: a bare `{ "output": ..., <producer fields> }` (the common case) writes
    /// `output` from any full `Producer`; `strip_prefix`/`unnest_sidepath_self`-shaped entries (see
    /// `InputTransformSpec`) are the few operations needing dynamic key iteration a single
    /// `Producer` output can't express.
    #[serde(default)]
    pub input_transforms: Vec<InputTransformSpec>,
    pub osm_fields: Vec<Field>,
    /// Simple field sanitizers: read one tag (or first present of several), clean it with a
    /// named `&str -> atomic` sanitizer, write to `tag`. Just sugar over `Field` — see
    /// `Field`'s `Deserialize` impl — so no distinct Rust type is needed downstream.
    #[serde(default)]
    pub sanitizers: Vec<Field>,
    /// References (by name) into the topic's `derivers.json` library. Each binding names a
    /// single-output deriver and the output field it writes. Categories may override these
    /// by re-binding a different deriver to the same output.
    #[serde(default)]
    pub derivers: Vec<DeriverBinding>,
    /// Optional Filter condition evaluated against raw way tags before categorization.
    /// If the condition matches, the way is skipped entirely for this topic.
    /// Uses the same Filter JSON syntax as category conditions.
    #[serde(default)]
    pub exclude_condition: Option<Filter>,
    /// Topic-level default constants seeded into `derived` (lowest priority — any sanitizer/
    /// deriver producing the same key overrides them). Categories override per-key via their own
    /// `consts`. A key starting with `_` routes into the `private` column instead of `derived`
    /// (the same convention `SplitContext::iter` uses for `_side`/`_prefix`/`_infix`) — there's no
    /// separate `private` map to declare one explicitly.
    #[serde(default)]
    pub consts: serde_json::Map<String, serde_json::Value>,
    /// Which geometry outputs this topic wants, per element kind — replaces the old global
    /// `--emit-way-geometries`/`--emit-relation-geometries`/`--topic-edges` CLI flags with a
    /// per-topic declaration. See `GeometryShape`.
    #[serde(default)]
    pub geometry: GeometrySpec,
}

/// Per-kind geometry output declarations (`topic.json`'s `"geometry"`). Node geometry (raw point)
/// isn't wired up yet — only `way`/`relation` shapes exist today.
#[derive(Debug, Deserialize, Default)]
pub struct GeometrySpec {
    /// Geometry outputs for this topic's ways.
    #[serde(default)]
    pub way: Vec<GeometryShape>,
    /// Geometry outputs for this topic's relations. Always a post-processing SQL step (Postgres
    /// output only) — a relation is classified before any member way's geometry is resolved (see
    /// `db::topic_geometries`), unlike `way`'s shapes, which are computed during streaming.
    #[serde(default)]
    pub relation: Vec<GeometryShape>,
}

/// One geometry output a topic can opt into for a given element kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GeometryShape {
    /// Ways only: this topic's kept ways feed into a per-topic `{table}_edge` pgRouting-shaped
    /// table (intersection-split, `cost`/`reverse_cost` from the topic's own `cost`/`is_directed`
    /// fields — see `db::topic_edges`). Requires the topic to define a `cost` field.
    Graph,
    /// The whole (unsplit) linestring per kept way, or one merged multi-linestring per kept
    /// relation (its member ways' geometries collected + line-merged) — see
    /// `db::topic_geometries`.
    Linestring,
}

/// One center-line split: unnest tags with `prefix` onto a side object whose effective highway
/// becomes `highway`. List the entry once per projection, e.g.
/// `{ "highway": "cycleway", "prefix": "cycleway", "directed_keys": ["cycleway:lanes", "bicycle:lanes"] }`.
/// `directed_keys` lists parent-way tags that are direction-sensitive (have `:forward`/`:backward`
/// variants); each is projected onto the side object, preferring the directed variant matching
/// that side, read from the *parent* way's tags. `self_directed_keys` is the same idea but read
/// from the side object's *own* tags instead — for tags that arrive on the object already suffixed
/// (e.g. a `cycleway:both:traffic_sign:forward` tag unnests to the object's own
/// `traffic_sign:forward` key, not the parent's).
#[derive(Debug, Deserialize)]
pub struct SplitSidesSpec {
    pub highway: String,
    pub prefix: String,
    #[serde(default)]
    pub directed_keys: Vec<String>,
    #[serde(default)]
    pub self_directed_keys: Vec<String>,
}

/// An `input_transforms` entry. Shape alone picks the variant (see `Deserialize` below) — no
/// `transform` discriminator to write.
#[derive(Debug)]
pub enum InputTransformSpec {
    /// Strip `prefix` from matching keys, re-key onto the base tag, and stamp a lifecycle-style
    /// marker (`<base>:<stamp_key>` when nested under one of `stamp_nested_under`, else `stamp_key`).
    /// The one step needing dynamic key iteration, so it isn't expressible as a bare `Producer`.
    /// Identified by its required `stamp_key` field.
    StripPrefix {
        prefix: String,
        stamp_key: String,
        stamp_value: String,
        stamp_nested_under: Vec<String>,
    },
    /// For ways whose own `highway` is a sidepath class (see the `sidepath_highway` value set),
    /// unnest bare `prefix`-prefixed tags (and their `source:`/`note:` meta variants) onto the way
    /// itself, plus derive `traffic_sign` from `traffic_sign:forward` for oneway cycleways. Models
    /// the OSM convention of tagging a way's own cycling function directly on it (e.g.
    /// `highway=path` + `cycleway=track`), as opposed to `split_sides` projecting side tags onto
    /// separate child objects. Also needs dynamic key iteration. Identified by being *only*
    /// `{ "prefix": ... }` — nothing else has that exact single-key shape.
    UnnestSidepathSelf { prefix: String },
    /// Write `output` from any full `Producer` (`rules`/`fallback`/`key`/`derive` — the same shape
    /// used for `osm_fields`/sanitizers/derivers), evaluated against the way's own tags (no
    /// parent, no category/side context yet) and applied as a raw-tag mutation *before*
    /// categorization — so, unlike a deriver, this can influence which category a way matches.
    /// A produced `null` deletes `output`; a produced non-null value must be a string and
    /// overwrites it; no match (`None`) leaves `output` untouched. The default (and by far most
    /// common) shape — identified by its required `output` field. e.g. deriving `traffic_sign`
    /// from `traffic_sign:forward` for oneway sidepaths:
    /// `{ "output": "traffic_sign", "rules": [
    ///      { "when": { "and": [ { "tag": "highway", "in_set": "sidepath_highway" },
    ///          { "tag": "traffic_sign", "exists": false }, { "tag": "oneway", "eq": "yes" },
    ///          { "not": { "tag": "oneway:bicycle", "eq": "no" } } ] },
    ///        "value": { "tag_or": "traffic_sign:forward", "or": "" } } ] }`.
    TagRules {
        output: String,
        source: Producer,
    },
}

impl<'de> serde::Deserialize<'de> for InputTransformSpec {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::Error;
        let v = serde_json::Map::deserialize(deserializer)?;
        if v.contains_key("stamp_key") {
            #[derive(Deserialize)]
            struct Repr {
                prefix: String,
                stamp_key: String,
                stamp_value: String,
                #[serde(default)]
                stamp_nested_under: Vec<String>,
            }
            let r: Repr = serde_json::from_value(Value::Object(v)).map_err(D::Error::custom)?;
            Ok(InputTransformSpec::StripPrefix {
                prefix: r.prefix,
                stamp_key: r.stamp_key,
                stamp_value: r.stamp_value,
                stamp_nested_under: r.stamp_nested_under,
            })
        } else if v.len() == 1 && v.contains_key("prefix") {
            let prefix = v["prefix"].as_str().ok_or_else(|| D::Error::custom("`prefix` must be a string"))?;
            Ok(InputTransformSpec::UnnestSidepathSelf { prefix: prefix.to_owned() })
        } else {
            let output = v
                .get("output")
                .and_then(Value::as_str)
                .ok_or_else(|| D::Error::custom("input_transforms entry needs an `output` field"))?
                .to_owned();
            let mut rest = v;
            rest.remove("output");
            let source = Producer::deserialize(Value::Object(rest)).map_err(D::Error::custom)?;
            Ok(InputTransformSpec::TagRules { output, source })
        }
    }
}

/// One produced field: `{ "output": ..., "source": <Producer> }`. Used for `osm_fields`,
/// desugared sanitizers, and resolved derivers alike — they all share one eval path.
///
/// A bare JSON string is shorthand for the common case of "read this tag verbatim, output under
/// the same name" — `"highway"` desugars to `{ "output": "highway", "source": { "key": "highway" } }`.
#[derive(Debug, Clone)]
pub struct Field {
    pub output: String,
    pub source: Producer,
}

impl<'de> serde::Deserialize<'de> for Field {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Repr {
            Named(String),
            Full { output: String, source: Producer },
            /// A simple sanitizer: `{ tag, name, in?, from? }`. Reads the first present of `in`
            /// (default `[tag]`) from `from` (default obj), applies the `name` sanitizer, writes
            /// to `tag`. Sugar for the equivalent `Producer::Extract`.
            Sanitizer {
                tag: String,
                name: String,
                #[serde(default, rename = "in")]
                in_keys: Option<StrOrVec>,
                #[serde(default)]
                from: TagSet,
            },
        }
        Ok(match Repr::deserialize(deserializer)? {
            Repr::Named(key) => Field {
                output: key.clone(),
                source: Producer::Extract {
                    key: Some(key),
                    keys: None,
                    from: TagSet::Obj,
                    side: None,
                    sanitize: None,
                    consts: serde_json::Map::new(),
                    directed: false,
                },
            },
            Repr::Full { output, source } => Field { output, source },
            Repr::Sanitizer { tag, name, in_keys, from } => Field {
                output: tag.clone(),
                source: Producer::Extract {
                    key: None,
                    keys: Some(in_keys.map(StrOrVec::into_vec).unwrap_or_else(|| vec![tag])),
                    from,
                    side: None,
                    sanitize: Some(SanitizeRef::Name(name)),
                    consts: serde_json::Map::new(),
                    directed: false,
                },
            },
        })
    }
}
