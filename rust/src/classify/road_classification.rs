use crate::osm::types::RawTags;

/// Port of RoadClassificationRoadValue.lua.
/// Returns the road classification string for a way.
pub fn road_classification_value(tags: &RawTags) -> Option<String> {
    let highway = tags.get("highway")?.as_str();

    let mut road_value = match highway {
        "road" => "unspecified_road",
        "steps" => "footway_steps",
        other => other,
    }
    .to_owned();

    // Sidewalks
    if (highway == "footway" && tags.get("footway").map(|v| v == "sidewalk").unwrap_or(false))
        || (highway == "path"
            && tags
                .get("is_sidepath")
                .map(|v| v == "yes")
                .unwrap_or(false))
        || (highway == "path"
            && tags
                .get("path")
                .map(|v| matches!(v.as_str(), "sidewalk" | "sidepath"))
                .unwrap_or(false))
    {
        road_value = "footway_sidewalk".into();
    }

    // Sidewalk crossings
    if highway == "footway"
        && tags
            .get("footway")
            .map(|v| matches!(v.as_str(), "crossing" | "traffic_island"))
            .unwrap_or(false)
    {
        road_value = "footway_crossing".into();
    }

    // Bikelane crossings
    if highway == "cycleway"
        && tags
            .get("cycleway")
            .map(|v| matches!(v.as_str(), "crossing" | "traffic_island"))
            .unwrap_or(false)
    {
        road_value = "cycleway_crossing".into();
    }

    // Foot and bicycle crossing
    if highway == "path"
        && tags
            .get("path")
            .map(|v| matches!(v.as_str(), "crossing" | "traffic_island"))
            .unwrap_or(false)
    {
        road_value = "footway_cycleway_crossing".into();
    }

    // Priority road (residential)
    if highway == "residential" {
        if tags
            .get("priority_road")
            .map(|v| matches!(v.as_str(), "designated" | "yes_unposted"))
            .unwrap_or(false)
        {
            road_value = "residential_priority_road".into();
        }
    }

    // Service sub-types
    if highway == "service" {
        road_value = match tags.get("service").map(String::as_str) {
            Some("alley") => "service_alley",
            Some("driveway") => "service_driveway",
            Some("emergency_access") => "service_emergency_access",
            Some("parking_isle") => "service_parking_aisle",
            None => "service_road",
            _ => "service_uncategorized",
        }
        .to_owned();

        // Emergency access overrides driveway
        if road_value == "service_driveway"
            && tags
                .get("emergency")
                .map(|v| matches!(v.as_str(), "yes" | "designated"))
                .unwrap_or(false)
        {
            road_value = "service_emergency_access".into();
        }
    }

    // Bicycle road
    if tags
        .get("bicycle_road")
        .map(|v| v == "yes")
        .unwrap_or(false)
    {
        road_value = "bicycle_road".into();
    }

    Some(road_value)
}
