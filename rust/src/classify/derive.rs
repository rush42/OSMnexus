//! Derivers: multi-input / context-dependent computations that produce a value from more
//! than one tag (and sometimes the matched category, side, or parent way). This is the
//! deliberate counterpart to `sanitize.rs`, whose functions are pure `&str -> atomic`.

use crate::osm::types::RawTags;
use crate::classify::sanitize::{first_present, sided_keys, SanitizerRegistry};

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

// ── shared helper ───────────────────────────────────────────────────────────────────

fn apply_str(reg: &SanitizerRegistry, name: &str, raw: &str) -> Option<String> {
    reg.apply(name, raw).and_then(|v| v.as_str().map(str::to_owned))
}

