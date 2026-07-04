//! Derivers: multi-input / context-dependent computations that produce a value from more
//! than one tag (and sometimes the matched category, side, or parent way). This is the
//! deliberate counterpart to `sanitize.rs`, whose functions are pure `&str -> atomic`.

use serde_json::Value;

use crate::classify::sanitize::{first_present, sided_keys, SanitizerRegistry};
use crate::engine::extract::{ExtractCtx, Produced};
use crate::osm::types::RawTags;

// ── parking inference (used by derive_traffic_mode) ─────────────────────────────

/// Returns `Some("parking")` when `parking:{side}` (or `:both`) is a known
/// non-"no" value, mirroring Lua's `inferTrafficModeFromParking`. The allow-list and
/// the `"no"`-collapse live in the data `parking` sanitizer (`{ cases: { parking: [...] },
/// on_miss: "drop" }`); only the sided tag selection stays here.
fn infer_traffic_mode_from_parking(
    tags: &RawTags,
    side: &str,
    reg: &SanitizerRegistry,
) -> Option<String> {
    let raw = first_present(tags, sided_keys("parking", side, false))?;
    apply_str(reg, "parking", raw)
}

/// Single-output port of Lua's `deriveTrafficMode`, computing one side's value.
/// `out_side` ("left"|"right") selects which side this deriver emits; `obj_side` is the
/// transformed object's side (from context), used for the directional inference branch.
/// `parking_inference` is the matched category's scope (`"both"` | `"directional"` | none),
/// which is data — the engine carries no category names.
///
/// Note: single-*output* ≠ single-*input* — the explicit-tag gate is cross-side (an explicit
/// `traffic_mode:*` on *either* side suppresses inference on both), so both sides are read.
/// Equivalent to the former tuple `derive_traffic_mode`, projected onto `out_side`.
pub fn traffic_mode_side(
    obj_tags: &RawTags,       // transformed object tags
    parking_tags: &RawTags,   // underlying way tags carrying parking:* (parent, or self for non-split)
    parking_inference: Option<&str>, // category's parking-inference scope: "both" | "directional"
    obj_side: &str,           // "left" | "right" | "self"
    out_side: &str,           // "left" | "right"
    reg: &SanitizerRegistry,
) -> Option<String> {
    // Sided, sanitized traffic_mode tag (normalize + allow-list via the data `traffic_mode`).
    let tm = |side: &str| {
        first_present(obj_tags, sided_keys("traffic_mode", side, true))
            .and_then(|raw| apply_str(reg, "traffic_mode", raw))
    };

    // Explicit tags win (on either side) — no inference; emit this side's explicit value.
    let explicit_any = tm("left").is_some() || tm("right").is_some();
    if explicit_any {
        return tm(out_side);
    }

    // Otherwise infer from the underlying way's parking tags, scoped by the category:
    //   "both"        → infer both sides (e.g. bicycle roads),
    //   "directional" → infer only the transformed side (e.g. on-highway cycleway lanes).
    match parking_inference {
        Some("both") => infer_traffic_mode_from_parking(parking_tags, out_side, reg),
        Some("directional") if obj_side == out_side => {
            infer_traffic_mode_from_parking(parking_tags, out_side, reg)
        }
        _ => None,
    }
}

/// `deriveBikelaneSmoothness`: re-evaluate the base `smoothness` fallback (the single source of
/// truth for the 4-source derivation + provenance) against own and parent tags, then copy the
/// parent's value under the Lua guards, prefixing its source with `parent_highway_`. Unlike
/// `traffic_mode_side` this one needs the extraction machinery (it re-runs a sibling `Producer`),
/// so it takes an `ExtractCtx`.
pub fn smoothness_parent(ctx: &ExtractCtx) -> Option<Produced> {
    let base = ctx.derivers.get("smoothness")?;
    let own = base.eval(ctx);

    let Some(parent) = ctx.parent_tags else { return own };
    let mut pctx = *ctx;
    pctx.obj_tags = parent;
    let par = base.eval(&pctx);
    if par.is_none() {
        return own;
    }

    let own_surface = ctx.obj_tags.get("surface");
    let surfaces_match = own_surface == parent.get("surface");
    let own_source = own.as_ref().and_then(|p| p.consts.get("source")).and_then(Value::as_str);
    let own_from_tag = matches!(own_source, Some("tag") | Some("tag_normalized"));

    // A: own absent, and own surface absent or equal to the parent's.
    let cond_a = own.is_none() && (own_surface.is_none() || surfaces_match);
    // B: own not tag-sourced (derived or absent), own surface present and equal.
    let cond_b = !own_from_tag && own_surface.is_some() && surfaces_match;

    if cond_a || cond_b {
        par.map(|mut p| {
            // Prefix the copied source with `parent_highway_`.
            if let Some(s) = p.consts.get("source").and_then(Value::as_str) {
                let prefixed = Value::String(format!("parent_highway_{s}"));
                p.consts.insert("source".into(), prefixed);
            }
            p
        })
    } else {
        own
    }
}

// ── shared helper ───────────────────────────────────────────────────────────────────

fn apply_str(reg: &SanitizerRegistry, name: &str, raw: &str) -> Option<String> {
    reg.apply(name, raw).and_then(|v| v.as_str().map(str::to_owned))
}

