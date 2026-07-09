use serde::Deserialize;
use crate::classify::categories::Filter;
use crate::classify::sanitize::StrOrVec;
use crate::engine::extract::{Producer, TagSet};

#[derive(Debug, Deserialize)]
pub struct TopicSpec {
    pub table: String,
    /// Ordered transform pipeline, e.g.
    /// `{ "transform": "split_sides", "sides": [{ "highway": ..., "prefix": ... }] }`. Applied in
    /// declaration order (see `PreCatStep`); `split_sides` is the exception, changing object
    /// cardinality rather than mutating tags in place.
    #[serde(default)]
    pub transforms: Vec<ParamTransform>,
    pub osm_fields: Vec<Field>,
    /// Simple field sanitizers: read one tag (or first present of several), clean it with a
    /// named `&str -> atomic` sanitizer, write to `tag`.
    #[serde(default)]
    pub sanitizers: Vec<Sanitizer>,
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
    /// `consts`.
    #[serde(default)]
    pub consts: serde_json::Map<String, serde_json::Value>,
    /// Topic-level default private metadata, emitted into the `private` column. Categories override
    /// per-key via their own `private`. The private counterpart to `consts`.
    #[serde(default)]
    pub private: serde_json::Map<String, serde_json::Value>,
}

/// A topic's `transforms` list, dispatched on the `transform` field.
/// `split_sides` changes object cardinality (handled by `side_split`); the rest are in-place
/// tag rewrites, unified into one ordered pre-categorization pass (see `PreCatStep`).
#[derive(Debug, Deserialize)]
#[serde(tag = "transform", rename_all = "snake_case")]
pub enum ParamTransform {
    /// One center-line split: unnest tags with `prefix` onto a side object whose effective
    /// highway becomes `highway`. List the entry once per projection, e.g.
    /// `{ "transform": "split_sides", "highway": "cycleway", "prefix": "cycleway",
    ///    "directed_keys": ["cycleway:lanes", "bicycle:lanes"] }`.
    /// `directed_keys` lists parent-way tags that are direction-sensitive (have `:forward`/
    /// `:backward` variants); each is projected onto the side object, preferring the directed
    /// variant matching that side, read from the *parent* way's tags.
    /// `self_directed_keys` is the same idea but read from the side object's *own* tags instead —
    /// for tags that arrive on the object already suffixed (e.g. a `cycleway:both:traffic_sign:
    /// forward` tag unnests to the object's own `traffic_sign:forward` key, not the parent's).
    SplitSides {
        highway: String,
        prefix: String,
        #[serde(default)]
        directed_keys: Vec<String>,
        #[serde(default)]
        self_directed_keys: Vec<String>,
    },
    /// For ways whose own `highway` is a sidepath class (see the `sidepath_highway` value set),
    /// unnest bare `prefix`-prefixed tags (and their `source:`/`note:` meta variants) onto the way
    /// itself, plus derive `traffic_sign` from `traffic_sign:forward` for oneway cycleways. Models
    /// the OSM convention of tagging a way's own cycling function directly on it (e.g.
    /// `highway=path` + `cycleway=track`), as opposed to `split_sides` projecting side tags onto
    /// separate child objects.
    UnnestSidepathSelf { prefix: String },
    /// Write `output` from any full `Producer` (`rules`/`fallback`/`key`/`derive` — the same
    /// shape used for `osm_fields`/sanitizers/derivers), evaluated against the way's own tags
    /// (no parent, no category/side context yet) and applied as a raw-tag mutation *before*
    /// categorization — so, unlike a deriver, this can influence which category a way matches.
    /// A produced `null` deletes `output`; a produced non-null value must be a string and
    /// overwrites it; no match (`None`) leaves `output` untouched. e.g. deriving `traffic_sign`
    /// from `traffic_sign:forward` for oneway sidepaths:
    /// `{ "transform": "tag_rules", "output": "traffic_sign", "rules": [
    ///      { "when": { "and": [ { "tag": "highway", "in_set": "sidepath_highway" },
    ///          { "tag": "traffic_sign", "exists": false }, { "tag": "oneway", "eq": "yes" },
    ///          { "not": { "tag": "oneway:bicycle", "eq": "no" } } ] },
    ///        "value": { "tag_or": "traffic_sign:forward", "or": "" } } ] }`.
    TagRules {
        output: String,
        #[serde(flatten)]
        source: Producer,
    },
    /// Strip `prefix` from matching keys, re-key onto the base tag, and stamp a lifecycle-style
    /// marker (`<base>:<stamp_key>` when nested under one of `stamp_nested_under`, else `stamp_key`).
    /// (Replaces `construction_prefix`.)
    StripPrefix {
        prefix: String,
        stamp_key: String,
        stamp_value: String,
        #[serde(default)]
        stamp_nested_under: Vec<String>,
    },
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
                },
            },
            Repr::Full { output, source } => Field { output, source },
        })
    }
}

/// A reference from `topic.json` (or a category) into the `derivers.json` library.
/// A bare string names a deriver whose output equals its name; the object form binds a
/// deriver to a different output (e.g. `surface_from_parent` → `surface`).
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum DeriverBinding {
    Named(String),
    Bound { deriver: String, output: String },
}

impl DeriverBinding {
    pub fn deriver(&self) -> &str {
        match self { DeriverBinding::Named(s) => s, DeriverBinding::Bound { deriver, .. } => deriver }
    }
    pub fn output(&self) -> &str {
        match self { DeriverBinding::Named(s) => s, DeriverBinding::Bound { output, .. } => output }
    }
}

/// A simple sanitizer: `{ tag, name, in?, from? }`. Reads the first present of `in` (default
/// `[tag]`) from `from` (default obj), applies the `name` sanitizer, writes to `tag`.
#[derive(Debug, Deserialize)]
pub struct Sanitizer {
    pub tag: String,
    pub name: String,
    #[serde(default, rename = "in")]
    pub in_keys: Option<StrOrVec>,
    #[serde(default)]
    pub from: TagSet,
}

impl Sanitizer {
    fn input_keys(&self) -> Vec<String> {
        match self.in_keys.clone() {
            Some(sv) => sv.into_vec(),
            None => vec![self.tag.clone()],
        }
    }

    /// Desugar to the equivalent `Field` so sanitizers and derivers share one eval path.
    pub fn to_field(&self) -> Field {
        Field {
            output: self.tag.clone(),
            source: Producer::Extract {
                key: None,
                keys: Some(self.input_keys()),
                from: self.from,
                side: None,
                sanitize: Some(self.name.clone()),
                consts: serde_json::Map::new(),
            },
        }
    }
}
