/// Bikelane minzoom: < 100m → 9, otherwise 0.
/// Port of BikeLaneGeneralization.lua.
pub fn bikelane_minzoom(length_m: f64) -> i32 {
    if length_m < 100.0 { 9 } else { 0 }
}

/// Road minzoom by classification.
pub fn road_minzoom(road_value: &str) -> i32 {
    match road_value {
        v if v.starts_with("primary") => 0,
        v if v.starts_with("secondary") => 0,
        v if v.starts_with("tertiary") => 0,
        "unclassified" => 0,
        "bicycle_road" => 0,
        "residential" | "residential_priority_road" => 9,
        "living_street" => 9,
        "road" | "unspecified_road" => 9,
        "pedestrian" => 9,
        v if v.starts_with("service") => 12,
        "track" => 12,
        "path" => 12,
        "footway" | "footway_sidewalk" | "footway_crossing" | "footway_steps" => 12,
        "cycleway" | "cycleway_crossing" | "footway_cycleway_crossing" => 9,
        _ => 12,
    }
}
