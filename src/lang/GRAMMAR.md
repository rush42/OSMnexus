# Tag-engine JSON grammar

This is the grammar for the JSON rule language read by `src/lang/*`
(`Producer`, `Filter`, `Extract`, `Sanitizer`) and by `src/topic/spec.rs`
(`topic.json` / `transforms.json`). It describes the *authored* shapes,
including all sugar forms; sugar is folded into the smaller runtime shape at
load/parse time (see "Desugaring" notes).

Notation: `Foo | Bar` = alternatives, tried in the given order when the shapes
overlap. `?` = optional field. `<Filter>` etc. reference another grammar
section. Untagged JSON objects are disambiguated by which fields are present.

## Extract

Reads a raw tag value and runs it through `sanitize` (identity if unset).
`sanitize` lives on `Extract` itself — every embedding site (`Filter`'s
tag/num predicates, `Producer::Extract`) pairs one `sanitize` with exactly
one `Extract` 1:1, so it's never a separate sibling field.

```
Extract =
  { "keys": [String, ...], "sanitize"?: SanitizeRef }   // aka "first_tag" (legacy alias) — first key found wins
  | { "key": String, "sanitize"?: SanitizeRef }          // aka "tag" (legacy alias)
```

## SanitizeRef

```
SanitizeRef =
  String                           // name, looked up in sanitizers.json
```

Always a name — there is no inline-chain form. Every sanitizer chain has
exactly one definition, in `sanitizers.json`; a `sanitize:` field that isn't a
bare string is a load-time error. Not a `Sanitizer` itself (see below) — it's
resolved away to one before any Rust type ever sees it.

## SanitizerChain / Sanitizer

`sanitizers.json` maps each name to a `SanitizerChain` — an ordered chain of
`Sanitizer` steps applied to a raw value. (Matches the Rust naming exactly:
`Sanitizer` = one step, `Vec<Sanitizer>` = a chain.)

```
SanitizerChain =
  [Sanitizer, ...]                 // explicit chain
  | Sanitizer                      // sugar: single-step chain

Sanitizer =
  { "mapping": {String: JSON, ...}, "on_miss"?: String }
  | { "cases": {String: [String,...] , ...}, "on_miss"?: String }
      // sugar: inverted-lookup mapping — desugars to "mapping"
  | { "filter": [String, ...] }
      // sugar: keep value iff member of set — desugars to "mapping" (on_miss: drop)
  | { "drop": [String, ...] }
      // sugar: drop value iff member of set — desugars to "mapping" (on_miss: "keep")
  | { "replace": [ReplaceRule, ...] }
  | String                          // built-in, e.g. "parse_length"

ReplaceRule = { "from": String, "to": String, "at"?: "anywhere" | "prefix" }
              // "at" defaults to "anywhere"
```

Built-in steps (looked up by name when `Sanitizer` is a bare string): `parse_length`.

`on_miss` controls what happens when a mapping lookup misses: `"keep"` keeps
the original value, `"drop"` (default for `cases`) drops it.

## Filter

A boolean predicate evaluated against a tag set. `Extract` fields below
(including its own `sanitize`) are inlined (`#[serde(flatten)]`) into the
same object as the comparison keyword.

```
Filter =
  Boolean                                          // literal true/false
  | { "and": [Filter, ...] }
  | { "or":  [Filter, ...] }
  | { "not": Filter }
  | { "macro": String }                            // expanded from macros.json before load
  | { <Extract fields>, "in_set": String }
  | { <Extract fields>, "in": [String, ...] }
  | { <Extract fields>, "contains": String, "case_insensitive"?: Boolean }
  | { <Extract fields>, "starts_with": String }
  | { <Extract fields>, "ends_with": String }
  | { <Extract fields>, "exists": Boolean }
  | { <Extract fields>, "eq": String }              // catch-all comparison
  | { "parent": Filter }                            // re-run filter against parent tags
  | { "annotation": String, "eq": String }          // annotations[annotation] == eq (e.g. "_side"/"_prefix"/"_infix")
  | { "has_key_prefix": String }
  | { "has_parent": Boolean }
  | { "tags_empty": Boolean }
  | { <Extract fields>, "lt":  Number }
  | { <Extract fields>, "lte": Number }
  | { <Extract fields>, "gt":  Number }
  | { <Extract fields>, "gte": Number }
```

Note: `<Extract fields>` means either `"key": String` (alias `"tag"`)
or `"keys": [String,...]` (alias `"first_tag"`) supplied inline in the same
object.

There is no `Cond`/`if-then-else` node. A conditional value is expressed as a
`Match` producer: a rule with a real `when:` filter for the "then" branch,
followed by an unconditional (`when: true`) trailing rule for "else".

## TagSet

Selects which tag scope a `Producer` reads from.

```
TagSet = "obj" (default) | "parent" | "parent_or_obj"
```

## Producer

Produces a JSON value from tags. Alternatives are tried in this order
(first shape that matches the object's fields wins):

```
Producer =
  { "parent": Producer }
      // re-evaluate the nested producer against parent tags

  | { "parent_or_obj": Producer }
      // sugar: try against obj tags; if no parent, fall back — desugars to a
      // 2-rule Match guarded by "has_parent"

  | { "fallback": [Producer, ...] }
      // sugar: first producer that yields a value wins — desugars to a Match
      // whose rules are all `when: true`, one per producer, "rules"-order

  | { "tag": String, "or"?: JSON }
      // sugar for Extract{key: tag}; with "or" present, a 1-rule Match, "default": or

  | { "rules": [Rule, ...], "default"?: JSON, "annotate"?: {String: JSON} }
      // Match: first matching rule wins; a rule that matches but yields
      // nothing does NOT stop the search (this is what "fallback" desugars into)

  | { <Extract fields>, "annotate"?: {String: JSON} }
      // Extract (tried after all the above, whose fields are all optional
      // and so would otherwise match everything first): reads + sanitizes a raw tag

  | JSON
      // Const (catch-all — tried last): a literal value, independent of any tag
```

There is no `{ "directed": ... }` `Producer` shape — a direction-sensitive read
moved to its own `transforms.json` pipeline step (`DirectedExtract`, see
below), never a runtime `Producer` variant.

`{ "shared": "name" }` is a pre-Producer reference, inlined from a shared
producer table before any `Producer` parsing happens (topic load time) — it
is not a `Producer` shape itself.

`annotate` are extra literal fields merged onto the produced value's metadata
(e.g. `{"source": "tag", "confidence": "high"}`) — used by attribution/UI, not
by matching. `Const` has no on-disk way to carry its own (a bare JSON literal
has nowhere to hang one) — as a `Match` rule's value it inherits the enclosing
`Match`'s `annotate` instead.

### Rule (used inside `Match`)

```
Rule = { "when": Filter, "value": Producer }
```

## `outputs` (topic.json)

Each entry in `topic.json`'s `outputs` map resolves to a `Producer`, trying
these shapes in order:

```
outputs.<name> =
  true
      // verbatim: Extract{ key: <name> }

  | String
      // named lookup: <name'> must exist in this topic's producers.json

  | { "name": String, "in"?: [String,...], "from"?: TagSet }
      // sanitizer shorthand (no key/keys/fallback/rules present):
      // Extract{ keys: in or [<name>] } piped through sanitize "name",
      // wrapped per "from" (obj default / parent / parent_or_obj)

  | Producer
      // full inline producer (any other object shape)
```

## `transforms.json`

A pipeline of steps run over raw tags before classification, split into
`before_exclude` / `after_exclude` phases around `exclude_condition`.

```
TransformsSpec = { "before_exclude": [PipelineStep, ...], "after_exclude": [PipelineStep, ...] }

PipelineStep =
  { "clone": Clone }
  | TransformStep

Clone = {
  "when"?: Filter,
  "annotate": {String: String, ...},
  "id_suffix": String,
  "steps": [TransformStep, ...]
}

TransformStep =
  { "prefix": String, "stamp_key": String, "stamp_value": String, "stamp_nested_under"?: [String,...] }
      // StripPrefix: strip a tag prefix, stamping stamp_key=stamp_value on
      // the resulting object (and optionally on nested-prefix children too)

  | { "unnest": String, "infix"?: String, "meta"?: [String,...], "guard"?: Filter, "record_infix_as"?: String }
      // Unnest: pull a `prefix:infix:...` tag subtree into its own object

  | { "drop": Filter }
      // Drop: discard the current object when Filter matches

  | { "output": String, "directed": { "key": String, "from"?: "obj" | "parent", "sanitize"?: SanitizeRef } }
      // DirectedExtract: side-aware tag read (used with left/right split ways).
      // "from" is the narrower Obj/Parent vocabulary, not the general TagSet —
      // identified by its required "directed" field, checked before TagRule

  | { "output": String, ...Producer fields... }
      // TagRule (catch-all): compute "output" via the embedded Producer
```

Disambiguation is by which distinguishing key is present: `stamp_key` →
StripPrefix, `unnest` → Unnest, `drop` → Drop, `directed` → DirectedExtract
(checked before the generic catch-all), else (requires `output`) → TagRule.
`PipelineStep` is `clone` vs. everything else (delegated to `TransformStep`).

## Worked example

```json
{
  "lifecycle": {
    "fallback": [
      { "key": "temporary", "sanitize": "temporary" },
      { "key": "lifecycle" },
      { "parent": { "key": "lifecycle" } }
    ]
  },
  "surface": {
    "fallback": [
      {
        "rules": [
          {
            "when": { "and": [
              { "tag": "surface", "eq": "sett" },
              { "num": "sett:length", "sanitize": "parse_length", "lte": 0.08 }
            ]},
            "value": "mosaic_sett"
          }
        ],
        "annotate": { "source": "tag", "confidence": "high" }
      },
      { "key": "surface", "sanitize": "surface_normalize", "annotate": { "source": "tag", "confidence": "high" } }
    ]
  }
}
```

## Source of truth

- `Extract` — `src/lang/extract.rs`
- `Filter` — `src/lang/filter.rs`
- `SanitizerChain` / `Sanitizer` — `src/lang/sanitize.rs` (`Sanitizer` = one step, `Vec<Sanitizer>`
  = a chain, matching the grammar names exactly). `SanitizeRef` (the field-level name reference)
  isn't a distinct Rust type — a named `sanitize: "<name>"` reference is resolved as a JSON-tree
  rewrite, `topic::load::inline_sanitize_refs`, before any Rust type ever sees it
- `Producer` / `Rule` — `src/lang/producer.rs`, `src/lang/classifier.rs`
- `DirectedExtract` — `src/topic/spec.rs` (`TransformSpec::DirectedExtract`),
  `src/categorize/transform.rs` (`InputTransform::DirectedExtract`, `DirectedFrom`)
- Sugar-folding `Deserialize` impls — `src/lang/parser.rs`
- `topic.json` / `transforms.json` schema — `src/topic/spec.rs`

See also memory: [[producer-engine-unified-to-match]], [[json-sugar-collapse-pattern]].
