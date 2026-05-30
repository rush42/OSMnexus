use std::collections::HashSet;

/// Motorways — excluded entirely from the pipeline.
pub fn highway_classes() -> HashSet<&'static str> {
    ["motorway", "motorway_link", "trunk", "trunk_link"]
        .into_iter()
        .collect()
}

pub fn major_road_classes() -> HashSet<&'static str> {
    [
        "primary",
        "primary_link",
        "secondary",
        "secondary_link",
        "tertiary",
        "tertiary_link",
    ]
    .into_iter()
    .collect()
}

pub fn minor_road_classes() -> HashSet<&'static str> {
    [
        "unclassified",
        "residential",
        "road",
        "living_street",
        "pedestrian",
        "service",
    ]
    .into_iter()
    .collect()
}

pub fn path_classes() -> HashSet<&'static str> {
    ["track", "path", "footway", "cycleway", "steps"]
        .into_iter()
        .collect()
}

/// path_classes ∪ {pedestrian} — used for sidepath detection.
pub fn sidepath_highway_classes() -> HashSet<&'static str> {
    let mut s = path_classes();
    s.insert("pedestrian");
    s
}

/// All allowed highway values (everything except motorways and unknown values).
pub fn allowed_highways() -> HashSet<&'static str> {
    let mut s = highway_classes();
    s.extend(major_road_classes());
    s.extend(minor_road_classes());
    s.extend(path_classes());
    s
}
