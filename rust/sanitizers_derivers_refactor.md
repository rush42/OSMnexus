# Plan: pure sanitizers + single-output derivers as referenceable libraries

## Goal

Two things at once:

1. **Restore lost derivation logic** in `surface` / `smoothness` (the Lua `DeriveSurface` /
   `DeriveSmoothness` chains were never fully ported — `surface` skips its sanitizer entirely,
   `smoothness` is missing the surface→/tracktype→/mtb:scale→ derivation sources; this is the
   known ~45% smoothness match).
2. **Clean up the architecture** so this logic is expressed declaratively and reused without
   duplication, and the smelly `if_copy_surface` flag disappears.

## Target architecture

Three layers, each with exactly one job.

### 1. `sanitizers.json` — pure 1→1 transforms (no I/O)

Each entry is a named transform `&str -> atomic` (or dropped). It never knows which tag feeds
it or where the result lands. A transform is one **step** or a **chain** (array of steps,
folded left, short-circuiting when a step drops the value):

```jsonc
{
  // step kinds:
  "yes_flag":               "yes_flag",            // bare string = built-in Rust fn
  "parse_length":           "parse_length",
  "surface":                "surface",             // built-in (reads sett:length for the size split)
  "surface_to_smoothness":  { "mapping": { "asphalt":"good", "cobblestone":"very_bad", ... },
                              "on_miss": "drop" },
  "tracktype_to_smoothness":{ "mapping": { "grade1":"good", ..., "grade5":"very_bad" },
                              "on_miss": "drop" },
  "smoothness_normalize":   [ { "filter": ["excellent","good","intermediate","bad","very_bad"] },
                              { "mapping": { "very_good":"excellent","impassable":"very_bad", ... },
                                "on_miss": "drop" } ],
  "separation":             [ { "mapping": { "kerb;tree_row":"tree_row", ... }, "on_miss":"keep" },
                              { "filter":  [ ...SEPARATION_ALLOWED... ] } ]
}
```

Step grammar:
- `{ "mapping": {k:v...}, "on_miss": "keep" | "drop" | "<const>" }` — table lookup; `on_miss`
  decides passthrough / null / default. (default `on_miss` = `drop`.)
- `{ "filter": [allowed...] }` — keep if in set, else drop (sugar for identity mapping + drop).
- `"<name>"` — a built-in Rust step from the registry (`parse_length`, `surface`,
  `traffic_sign`, `yes_flag`, `temporary`, `buffer`, `mtb_scale_to_smoothness`, …): the escape
  hatch for anything that isn't a finite table (unit parsing, multi-tag reads, thresholds).
- an **array** of the above is a chain.

The sanitizer is unambiguously **1→1**: it always receives a single value.

### 2. `derivers.json` — named single-output N→1 extractors (no output field name)

A deriver combines *semantically distinct* sources (first-non-empty wins), each source
optionally run through a sanitizer. It is **output-agnostic** — the output field name is bound
later in `topic.json`. Every deriver produces **exactly one** field.

```jsonc
{
  "surface":             { "fallback": [ { "key":"surface", "sanitize":"surface" } ] },
  "surface_from_parent": { "fallback": [ { "key":"surface", "sanitize":"surface" },
                                         { "key":"surface", "from":"parent", "sanitize":"surface" } ] },

  "smoothness":          { "fallback": [
                              { "key":"smoothness", "sanitize":"smoothness_normalize" },
                              { "key":"surface",    "sanitize":"surface_to_smoothness" },
                              { "key":"tracktype",  "sanitize":"tracktype_to_smoothness" },
                              { "key":"mtb:scale",  "sanitize":"mtb_scale_to_smoothness" } ] },
  "smoothness_from_parent": { "fallback": [ /* own 4 sources + guarded parent step (Phase 3) */ ] },

  "lifecycle":           { "fallback": [ { "key":"temporary", "sanitize":"temporary" },
                                         { "key":"lifecycle" },
                                         { "key":"lifecycle", "from":"centerline" } ] },

  "oneway":              { "derive": "oneway" },                       // Rust-backed
  "traffic_mode_left":   { "derive": "traffic_mode", "out_side": "left"  },
  "traffic_mode_right":  { "derive": "traffic_mode", "out_side": "right" }
}
```

A source = `{ key | keys, from = obj|parent|parent_or_obj|centerline, side?, sanitize? }`.
`derive` names a Rust deriver, with any fixed args (e.g. `out_side`).

### 3. `topic.json` — the wiring (and category overrides)

```jsonc
"sanitizers": [                                  // bind a 1→1 transform to in/out
  { "sanitize":"parse_length", "in":"width" },                       // out defaults to in ("width")
  { "sanitize":"parse_length", "in":"width:effective", "out":"width_effective" },
  { "sanitize":"separation",   "in":["separation:left","separation:both","separation"],
                               "out":"separation_left" }              // `in` list = first-present
],
"derivers": [                                    // bind a single-output deriver to an output
  "oneway", "lifecycle", "surface", "smoothness", // bare string: output = deriver name
  "traffic_mode_left", "traffic_mode_right"
]
```

- **sanitizer binding**: `{ sanitize, in (single key | first-present list), out }`, `out`
  defaults to `in`. The first-present list is the same fallback primitive as derivers, kept as
  binding sugar so synonym reads stay one-liners.
- **deriver binding**: bare string (output = name) or `{ deriver, output }`.
- `osm_fields` stay inline in `topic.json` (nothing overrides them per-category yet).

**Category overrides** (in `categories/*.json`) re-bind a different deriver to an output:

```jsonc
// a copySurfaceSmoothnessFromParent category:
"derivers": [ { "deriver":"surface_from_parent",    "output":"surface" },
              { "deriver":"smoothness_from_parent", "output":"smoothness" } ]
```

Resolution: topic derivers → map by `output` → category derivers replace by `output` (category
wins — exactly the existing minzoom precedent in `runner.rs:179`).

### Sharing across topics (Phase 2 lift)

`bridge`, `tunnel`, `traffic_sign`, `lifecycle` are identical across topics. Resolver looks up
a name in the topic-local file, then falls back to `_shared/sanitizers.json` /
`_shared/derivers.json` (topic-local wins — same merge rule as the existing `_shared/` filter
macros). Build per-topic first; lift the shared ones afterwards. Design the resolver to allow
it from the start.

## What gets deleted

- `Producer::Extract.if_copy_surface` (and the `if *if_copy_surface && !ctx...` gate in
  `extract.rs`).
- `ExtractCtx.copy_surface_from_parent`.
- The parent-copy is now purely structural: only copy-categories reference
  `surface_from_parent` / `smoothness_from_parent`.

## What stays as Rust (escape hatch, named built-in steps/derivers)

- `surface` sanitizer: the transformation table is data, but the **`sett` size split** (reads
  `sett:length`, `parse_length` thresholds → `mosaic`/`small`/`large_sett`) is a Rust step.
- `parse_length`, `buffer`, `traffic_sign`, `temporary`, `yes_flag`, `mtb_scale_to_smoothness`.
- `oneway`, `traffic_mode` derivers.

## traffic_mode split (single-output)

`traffic_mode` is the only current multi-output deriver. Split into `traffic_mode_left` /
`traffic_mode_right`, each fixing `out_side`. **Single-output ≠ single-input** — each per-side
deriver must still:
1. read *both* sides' explicit tags (explicit on either side suppresses inference on both —
   `derive.rs:50`), and
2. use the object's transformed side from context for the `DIRECTIONAL` inference branch
   (`derive.rs:63`), distinct from the output side.

```rust
fn traffic_mode_side(obj, centerline, category, obj_side, out_side) -> Option<String> {
    let explicit_any = traffic_mode(obj,"left").is_some() || traffic_mode(obj,"right").is_some();
    if explicit_any { return traffic_mode(obj, out_side); }
    if is_bicycle_road(category) { return infer(centerline, out_side); }
    if DIRECTIONAL.contains(category) && obj_side == out_side { return infer(centerline, out_side); }
    None
}
```
Verified equivalent to the existing tuple fn on every branch. `derivers.json` fixes `out_side`;
`obj_side` comes from `ctx`.

## Logic restored (Lua reference)

- **surface** (`DeriveSurface` + `sanitize_tags.surface`): transformation table
  (`earth/mud/clay/dirt→ground`, `cobblestone/unhewn_cobblestone/cobblestone:flattened→large_sett`,
  `rock/stone:plates→stone`, `paving_stones:20/:30→paving_stones`, `tartan→rubber`), the `sett`
  size split, then the allow-list (disallowed → drop). Applied to own tag; `surface_from_parent`
  applies the same to the parent.
- **smoothness** (`DeriveSmoothness`): 4-source fallback — normalize own `smoothness`
  (allow-list `{excellent,good,intermediate,bad,very_bad}`; map `very_good→excellent`,
  `impassable/horrible/very_horrible→very_bad`) → `surface` via the big `surfaceToSmoothness`
  table (+ non-standard table) → `tracktype` (`grade1..5`) → `mtb:scale` (`0/0+/0-→bad`, else
  `very_bad`).
- **smoothness parent copy** (`deriveBikelaneSmoothness`, lines 20–32) — the hard part: copy
  parent smoothness only when own smoothness is absent (and own surface absent or == parent
  surface), OR own smoothness is non-tag-sourced and own surface == parent surface and parent
  has explicit smoothness. Needs to know whether own smoothness came from a tag vs. was derived
  → likely a Rust deriver / source tracking (touches the parked source/confidence idea in
  [[fallback-sanitizer-idea]]). Approximate first, tighten here.

Note: `surface_source`/`surface_confidence` / `smoothness_source` emission is still out of scope
(parked) — restore values first.

## Implementation steps

**Phase 1 — config restructure (behavior-preserving).**
- Add `sanitizers.json` / `derivers.json` loading + name-reference resolution in `TopicRunner`,
  with **load-time validation** (hard error on a dangling name).
- Move current definitions into the libraries *as-is* (keep today's producers, including the
  current centerline-based surface/smoothness copy reproduced as `surface_from_parent` /
  `smoothness_from_parent`).
- Implement category deriver overrides (replace-by-output). Map every category with
  `copy_surface_smoothness_from_parent = true` to reference the `_from_parent` variants; remove
  the flag, `if_copy_surface`, and `copy_surface_from_parent`.
- Split `traffic_mode` into `traffic_mode_left` / `traffic_mode_right`.
- **Verify: self-diff Berlin byte-identical** (this phase must not change output).

**Phase 2 — sanitizer chain engine + restore surface/smoothness (behavior-changing).**
- Implement `mapping` / `filter` steps + chaining in `apply_sanitizer` (or a new evaluator),
  keeping the built-in named steps.
- Port the `surface`, `surface_to_smoothness`, `tracktype_to_smoothness`,
  `mtb_scale_to_smoothness`, `smoothness_normalize` tables; wire the 4-source `smoothness`
  deriver and the sanitized `surface` deriver.
- **Verify vs `osm_lua`**: smoothness ~45% → parity; surface tighter; other columns unchanged.

**Phase 3 — smoothness parent-copy guards.**
- Encode `deriveBikelaneSmoothness` conditions (Rust deriver or conditional producer).
- **Verify vs `osm_lua`** again.

## Verification method

Per-id comparison against `osm_lua` (not just category counts — OSM data drift inflates raw row
diffs). Phase 1 uses a self-diff baseline in the `osm` DB and must be 0 across all columns.
