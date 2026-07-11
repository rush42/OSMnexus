//! Generic access to the named unit-conversion tables in `<config_root>/units.json`, plus the
//! one general algorithm that reads them: `parse_compound_unit`.
//!
//! This module holds no domain knowledge — which units exist and their factors relative to the
//! table's base unit (e.g. metres for `"length"`) live entirely in the JSON. What's generic here
//! is the parse itself: repeatedly consume a leading number plus an optional unit suffix, convert
//! each to the base unit, and sum — which handles both a plain single-unit value ("2.5 m",
//! "250 cm") and a compound one ("8'6\"" = 8 ft + 6 in) with the same algorithm.

use std::collections::HashMap;
use std::sync::OnceLock;

fn all_units() -> &'static HashMap<String, Vec<(String, f32)>> {
    static UNITS: OnceLock<HashMap<String, Vec<(String, f32)>>> = OnceLock::new();
    UNITS.get_or_init(|| {
        let path = crate::paths::config_root().join("units.json");
        let raw = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
        serde_json::from_str(&raw).unwrap_or_else(|e| panic!("parsing {}: {e}", path.display()))
    })
}

/// The named unit table (`[(suffix, factor), ...]`), panicking if the JSON doesn't define it.
/// Order matters: matching is first-suffix-wins against the string immediately following each
/// number, so a suffix that's a prefix of another (e.g. `"m"` vs `"mm"`) must be declared *after*
/// the longer one, or it'll shadow it.
pub fn unit_table(name: &str) -> &'static [(String, f32)] {
    all_units()
        .get(name)
        .unwrap_or_else(|| panic!("units.json: no unit table named '{name}'"))
        .as_slice()
}

/// Parse a string made of one or more `<number><unit>` runs (optionally whitespace-separated,
/// e.g. `"8'6\""`, `"250 cm"`, `"2.5"`), converting each run to `units`' base unit via its factor
/// and summing. A run with no matching unit suffix is treated as already being in the base unit
/// (only sensible at the end of the string). Accepts `,` as a decimal separator. Returns `None` on
/// anything that doesn't parse as at least one such run (empty input, or trailing unparseable text).
pub fn parse_compound_unit(raw: &str, units: &[(String, f32)]) -> Option<f32> {
    let mut s = raw.trim();
    if s.is_empty() {
        return None;
    }
    let mut total = 0.0_f32;
    while !s.is_empty() {
        let num_end = s.find(|c: char| !(c.is_ascii_digit() || c == '.' || c == ',')).unwrap_or(s.len());
        if num_end == 0 {
            return None; // no number where one was expected
        }
        let value: f32 = s[..num_end].replace(',', ".").parse().ok()?;
        s = s[num_end..].trim_start();

        let factor = match units.iter().find(|(suffix, _)| s.starts_with(suffix.as_str())) {
            Some((suffix, factor)) => {
                s = s[suffix.len()..].trim_start();
                *factor
            }
            None => 1.0, // no unit left to match — treat the remainder as the base unit
        };
        total += value * factor;
    }
    Some(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn length_units() -> Vec<(String, f32)> {
        vec![
            ("km".into(), 1000.0), ("cm".into(), 0.01), ("mm".into(), 0.001),
            ("ft".into(), 0.3048), ("'".into(), 0.3048), ("\"".into(), 0.0254), ("m".into(), 1.0),
        ]
    }

    fn parse(raw: &str) -> Option<f32> {
        parse_compound_unit(raw, &length_units())
    }

    #[test]
    fn bare_number_is_metres() {
        assert_eq!(parse("2.5"), Some(2.5));
    }

    #[test]
    fn single_unit_suffix() {
        assert_eq!(parse("2.5 m"), Some(2.5));
        assert_eq!(parse("250 cm"), Some(2.5));
        assert_eq!(parse("2500 mm"), Some(2.5));
        assert_eq!(parse("8 ft"), Some(2.4384));
        assert_eq!(parse("1km"), Some(1000.0));
    }

    #[test]
    fn feet_and_inches_compound() {
        assert_eq!(parse("8'6\""), Some(2.5908));
        assert_eq!(parse("8'"), Some(2.4384));
    }

    #[test]
    fn comma_decimal_separator() {
        assert_eq!(parse("2,5"), Some(2.5));
    }

    #[test]
    fn empty_or_garbage_is_none() {
        assert_eq!(parse(""), None);
        assert_eq!(parse("abc"), None);
    }

    #[test]
    fn mm_not_shadowed_by_m() {
        // "m" is a prefix of "mm" — the table must match the longer suffix first. (Not exactly
        // 0.005: f32 can't represent it exactly, same as the original hand-rolled parser.)
        assert!((parse("5mm").unwrap() - 0.005).abs() < 1e-6);
    }
}
