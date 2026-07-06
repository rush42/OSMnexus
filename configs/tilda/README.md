# tilda config — pipeline limitations

The rules in this config folder (`topic.json`, `way/*.json`, `macros.json`,
`sanitizers.json`, `derivers.json`, `_shared/*`) are all evaluated by the
classification stage (`src/classify/filter.rs`). That stage sees **tags
only** — it has no access to geometry. Keep the following limitations in
mind when writing or reviewing rules here:

## No geometry predicates

There is no predicate that reads geometry-derived values. Concretely, you
cannot write a rule that depends on:

- **Length** of a way (e.g. "only classify as X if the segment is shorter
  than N meters"). Length-based filtering has to happen later, in the
  geometry/graph stage — not in this config.
- **Area** (for closed ways/polygons).
- Any other geometry predicate: shape, curvature, intersection angle,
  proximity/distance to other features, point-in-polygon tests, etc.

`Filter::NumLt`/`NumGt`/etc. (the `num` predicates) only read a *tag* value
(optionally through a `sanitize` chain, e.g. `parse_length` to turn
`"50 m"` into a number) and compare it numerically. They do **not** compute
anything from the actual geometry of the OSM way/node/relation — a way with
no `length`-like tag simply has no numeric value to compare, regardless of
its real-world length.

## What predicates ARE available

Classification (`Filter` in `src/classify/filter.rs`) can only express:

- Tag equality/membership/prefix/suffix/contains (`eq`, `in`, `in_set`,
  `starts_with`, `ends_with`, `contains`, `exists`), optionally on the first
  present tag from a list (`first_tag`), or on the parent way's tags
  (`parent_tag`) for left/right-split objects.
- Numeric comparisons on a *tag's* value (`num` + `lt`/`lte`/`gt`/`gte`),
  optionally sanitized first.
- Context predicates: `side` (self/left/right), `prefix`, `infix`,
  `has_key_prefix`, `has_parent`.
- Combinators: `and`, `or`, `not`, and named `macro` references.

If a rule needs something not on this list — most commonly a geometric
property — it cannot be expressed in these JSON configs today; it needs a
change to the Rust engine (a new geometry-aware stage) rather than a new
JSON rule.
