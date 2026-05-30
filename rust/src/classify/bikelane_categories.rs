use crate::osm::types::RawTags;
use crate::output::types::Side;

/// Context passed to category condition functions.
/// Replaces all `_`-prefixed private tag access from the Lua pipeline.
pub struct CategoryContext<'a> {
    pub tags: &'a RawTags,
    pub side: Side,
    pub prefix: Option<&'a str>,
    /// Original highway value of the parent way (set for left/right transformed objects).
    pub parent_highway: Option<&'a str>,
    /// Tags of the parent way (set for left/right transformed objects).
    pub parent_tags: Option<&'a RawTags>,
    /// The infix that matched during side splitting (e.g. "", "left", "both").
    pub infix: Option<&'a str>,
    pub length_m: f64,
}

pub struct BikelaneCategory {
    pub id: &'static str,
    pub infrastructure_exists: bool,
    pub implicit_oneway: bool,
    pub implicit_oneway_confidence: &'static str,
    pub copy_surface_smoothness_from_parent: bool,
    pub condition: fn(&CategoryContext) -> bool,
}

// ── Helpers ──────────────────────────────────────────────────────────────────

fn tag<'a>(ctx: &'a CategoryContext<'a>, key: &str) -> Option<&'a str> {
    ctx.tags.get(key).map(String::as_str)
}

fn tag_is(ctx: &CategoryContext, key: &str, val: &str) -> bool {
    ctx.tags.get(key).map(|v| v == val).unwrap_or(false)
}

fn tag_in(ctx: &CategoryContext, key: &str, vals: &[&str]) -> bool {
    ctx.tags.get(key).map(|v| vals.contains(&v.as_str())).unwrap_or(false)
}

fn parent_tag<'a>(ctx: &'a CategoryContext<'a>, key: &str) -> Option<&'a str> {
    ctx.parent_tags.and_then(|t| t.get(key)).map(String::as_str)
}

fn sign_contains(sign: Option<&str>, needle: &str) -> bool {
    sign.map(|s| s.contains(needle)).unwrap_or(false)
}

fn sign_starts_with(sign: Option<&str>, prefix: &str) -> bool {
    sign.map(|s| s.starts_with(prefix)).unwrap_or(false)
}

/// Port of `IsSidepath` from IsSidepath.lua.
fn is_sidepath(ctx: &CategoryContext) -> bool {
    // Explicit override: is_sidepath=no beats everything, including parent_highway.
    if tag_is(ctx, "is_sidepath", "no") {
        return false;
    }
    tag_is(ctx, "is_sidepath", "yes")
        || ctx.parent_highway.is_some()
        || tag_is(ctx, "footway", "sidewalk")
        || tag_is(ctx, "path", "sidewalk")
        || tag_is(ctx, "path", "sidepath")
        || tag_is(ctx, "cycleway", "sidepath")
        || tag_is(ctx, "steps", "sidewalk")
}

/// Port of `is_crossing_pattern` from BikelaneCategories.lua.
fn is_crossing_pattern(ctx: &CategoryContext) -> bool {
    let hw = tag(ctx, "highway");
    let cycleway = tag(ctx, "cycleway");

    if hw == Some("cycleway") && cycleway == Some("lane") && tag_is(ctx, "lane", "crossing") {
        return true;
    }
    if hw == Some("cycleway") && matches!(cycleway, Some("crossing") | Some("traffic_island")) {
        return true;
    }
    if hw == Some("path")
        && matches!(tag(ctx, "path"), Some("crossing") | Some("traffic_island"))
        && tag_in(ctx, "bicycle", &["yes", "designated"])
    {
        return true;
    }
    if hw == Some("footway")
        && matches!(tag(ctx, "footway"), Some("crossing") | Some("traffic_island"))
        && tag_in(ctx, "bicycle", &["yes", "designated"])
    {
        return true;
    }
    false
}

/// Returns true if this side object is part of a "BetweenLanes" dual-tagging situation.
/// Port of hasCyclewayOnHighwayBetweenLanesConditions from BikelaneCategories.lua.
fn has_between_lanes_conditions(ctx: &CategoryContext) -> bool {
    if ctx.side == Side::Self_ {
        return false;
    }
    // `lanes` is the unnested value of `cycleway:lanes` on the side object.
    if sign_contains(tag(ctx, "lanes"), "|lane|") {
        return true;
    }
    // `bicycle:lanes` on the parent way (not unnested onto side object).
    if sign_contains(parent_tag(ctx, "bicycle:lanes"), "|designated|") {
        return true;
    }
    false
}

/// Base condition for cyclewayOnHighway_advisoryOrExclusive.
fn is_advisory_or_exclusive(ctx: &CategoryContext) -> bool {
    if !tag_is(ctx, "highway", "cycleway") {
        return false;
    }
    if !tag_in(ctx, "cycleway", &["lane", "opposite_lane"]) {
        return false;
    }
    // Guard: when this side object is part of dual-tagged BetweenLanes setup,
    // only accept it if the lane tag ends the lanes string (i.e. it's the edge lane).
    // Lua: `if ContainsSubstring(tags.lanes, '|lane|') and not has_suffix(tags.lanes, '|lane') then return false`
    if has_between_lanes_conditions(ctx) {
        let lanes = tag(ctx, "lanes").unwrap_or("");
        let bicycle_lanes = parent_tag(ctx, "bicycle:lanes").unwrap_or("");
        if lanes.contains("|lane|") && !lanes.ends_with("|lane") {
            return false;
        }
        if bicycle_lanes.contains("|designated|") && !bicycle_lanes.ends_with("|designated") {
            return false;
        }
    }
    true
}

/// Base condition for bicycleRoad.
fn is_bicycle_road(ctx: &CategoryContext) -> bool {
    if tag_is(ctx, "bicycle_road", "yes") {
        return true;
    }
    sign_starts_with(tag(ctx, "traffic_sign"), "DE:244")
}

/// Base condition for footAndCyclewayShared.
fn is_foot_and_cycleway_shared_base(ctx: &CategoryContext) -> bool {
    if is_crossing_pattern(ctx) {
        return false;
    }
    let hw = tag(ctx, "highway").unwrap_or("");
    let sign = tag(ctx, "traffic_sign");

    if hw == "cycleway" && tag_is(ctx, "cycleway", "track") {
        if tag_is(ctx, "segregated", "no") || sign_contains(sign, "240") {
            return true;
        }
    }
    let allowed = &["cycleway", "path", "footway", "service", "track"];
    if allowed.contains(&hw) {
        // Lua requires both values to match: both "designated" OR both "yes".
        // bicycle=yes + foot=designated does NOT match.
        if tag_is(ctx, "segregated", "no")
            && ((tag_is(ctx, "bicycle", "designated") && tag_is(ctx, "foot", "designated"))
                || (tag_is(ctx, "bicycle", "yes") && tag_is(ctx, "foot", "yes")))
        {
            return true;
        }
        if sign_contains(sign, "240") {
            return true;
        }
    }
    false
}

/// Base condition for footAndCyclewaySegregated.
fn is_foot_and_cycleway_segregated_base(ctx: &CategoryContext) -> bool {
    if is_crossing_pattern(ctx) {
        return false;
    }
    let hw = tag(ctx, "highway").unwrap_or("");
    let sign = tag(ctx, "traffic_sign");

    if hw == "cycleway" && tag_is(ctx, "cycleway", "track") {
        if tag_is(ctx, "segregated", "yes") || sign_contains(sign, "241") {
            return true;
        }
    }
    if matches!(hw, "cycleway" | "path" | "footway") {
        // Same matching-pair rule as footAndCyclewayShared: both "designated" OR both "yes".
        if tag_is(ctx, "segregated", "yes")
            && ((tag_is(ctx, "bicycle", "designated") && tag_is(ctx, "foot", "designated"))
                || (tag_is(ctx, "bicycle", "yes") && tag_is(ctx, "foot", "yes")))
        {
            return true;
        }
        if sign_contains(sign, "241") && hw != "footway" {
            return true;
        }
    }

    // Edge case: separate geometry with foot traffic on the right side but no segregated tag.
    // Lua reads tags['traffic_mode:right'] via SANITIZE_ROAD_TAGS.traffic_mode(tags, 'right').
    // See https://www.openstreetmap.org/way/1319011143
    if hw == "cycleway" {
        let tm_right = tag(ctx, "traffic_mode:right")
            .or_else(|| tag(ctx, "traffic_mode:both"));
        if tm_right.map(|v| matches!(v, "foot" | "foot;bicycle")).unwrap_or(false) {
            let sep_right = tag(ctx, "separation:right")
                .or_else(|| tag(ctx, "separation:both"));
            // Lua normalizes "surface" and "lane_separator" → "no" (paint ≠ physical separation).
            let sep_normalized = sep_right.map(|v| match v {
                "surface" | "lane_separator" => "no",
                other => other,
            });
            let separation_ok = sep_normalized.is_none() || sep_normalized == Some("no");
            if separation_ok {
                return true;
            }
        }
    }

    false
}

/// Base condition for cyclewaySeparated.
fn is_cycleway_separated_base(ctx: &CategoryContext) -> bool {
    if tag_is(ctx, "cycleway", "lane") || is_crossing_pattern(ctx) {
        return false;
    }
    let hw = tag(ctx, "highway").unwrap_or("");
    let cycleway = tag(ctx, "cycleway");
    let sign = tag(ctx, "traffic_sign");

    if hw == "cycleway"
        && matches!(cycleway, Some("track") | Some("opposite_track"))
    {
        return true;
    }
    // Lua truthy check: any non-nil is_sidepath value (including "no") fires this.
    // is_sidepath=no then sends it to _isolated via the subcategory condition.
    if hw == "cycleway" && tag(ctx, "is_sidepath").is_some() {
        return true;
    }

    let allowed = &[
        "living_street",
        "pedestrian",
        "service",
        "track",
        "bridleway",
        "path",
        "footway",
        "cycleway",
    ];
    if allowed.contains(&hw) && sign_contains(sign, "237") {
        return true;
    }
    false
}

/// Base condition for footwayBicycleYes.
fn is_footway_bicycle_yes_base(ctx: &CategoryContext) -> bool {
    if is_crossing_pattern(ctx) {
        return false;
    }
    let hw = tag(ctx, "highway").unwrap_or("");
    if !matches!(hw, "footway" | "path") {
        return false;
    }
    let has_bicycle_access = tag_is(ctx, "bicycle", "yes")
        || sign_contains(tag(ctx, "traffic_sign"), "1022-10");
    if !has_bicycle_access {
        return false;
    }
    if let Some(mtb) = tag(ctx, "mtb:scale") {
        let cleaned: String = mtb.chars().filter(|c| !matches!(c, '+' | '-' | ' ')).collect();
        match cleaned.parse::<f64>() {
            Ok(n) if n > 1.0 => return false,
            Err(_) => return false,
            _ => {}
        }
        if tag(ctx, "traffic_sign").is_none() && tag(ctx, "is_sidepath").is_none() {
            return false;
        }
    }
    true
}

// ── Category definitions ──────────────────────────────────────────────────────

const DATA_NO: BikelaneCategory = BikelaneCategory {
    id: "data_no",
    infrastructure_exists: false,
    implicit_oneway: false,
    implicit_oneway_confidence: "not_applicable",
    copy_surface_smoothness_from_parent: false,
    condition: |ctx| tag_in(ctx, "cycleway", &["no", "none"]),
};

const SEPARATE_GEOMETRY: BikelaneCategory = BikelaneCategory {
    id: "separate_geometry",
    infrastructure_exists: false,
    implicit_oneway: false,
    implicit_oneway_confidence: "not_applicable",
    copy_surface_smoothness_from_parent: false,
    condition: |ctx| tag_is(ctx, "cycleway", "separate"),
};

const NOT_EXPECTED: BikelaneCategory = BikelaneCategory {
    id: "not_expected",
    infrastructure_exists: false,
    implicit_oneway: false,
    implicit_oneway_confidence: "not_applicable",
    copy_surface_smoothness_from_parent: false,
    condition: |ctx| {
        ctx.prefix == Some("cycleway")
            && ctx.infix == Some("")
            && ctx.side == Side::Left
            && parent_tag(ctx, "oneway") == Some("yes")
            && parent_tag(ctx, "oneway:bicycle") != Some("no")
    },
};

const CYCLEWAY_ON_HIGHWAY_PROTECTED: BikelaneCategory = BikelaneCategory {
    id: "cyclewayOnHighwayProtected",
    infrastructure_exists: true,
    implicit_oneway: true,
    implicit_oneway_confidence: "medium",
    copy_surface_smoothness_from_parent: true,
    condition: |ctx| {
        if is_crossing_pattern(ctx) {
            return false;
        }
        if !is_sidepath(ctx) || tag_is(ctx, "lane", "advisory") {
            return false;
        }
        if tag_in(ctx, "cycleway", &["share_busway", "opposite_share_busway"]) {
            return false;
        }
        let allowed_sep = &[
            "bollard", "flex_post", "vertical_panel", "studs", "bump",
            "planter", "fence", "jersey_barrier", "guard_rail",
        ];
        // Check separation:left
        let sep_left = tag(ctx, "separation:left")
            .or_else(|| tag(ctx, "separation:both"))
            .or_else(|| tag(ctx, "separation"));
        if sep_left.map(|v| allowed_sep.contains(&v)).unwrap_or(false) {
            if tag_is(ctx, "segregated", "yes") || tag_is(ctx, "segregated", "no") {
                return false;
            }
            return true;
        }
        // Parked cars left
        if tag_in(ctx, "traffic_mode:left", &["parking"]) {
            if tag(ctx, "segregated").is_some() {
                return false;
            }
            return true;
        }
        false
    },
};

const CYCLEWAY_LINK: BikelaneCategory = BikelaneCategory {
    id: "cyclewayLink",
    infrastructure_exists: true,
    implicit_oneway: true,
    implicit_oneway_confidence: "low",
    copy_surface_smoothness_from_parent: false,
    condition: |ctx| tag_is(ctx, "highway", "cycleway") && tag_is(ctx, "cycleway", "link"),
};

const CROSSING: BikelaneCategory = BikelaneCategory {
    id: "crossing",
    infrastructure_exists: true,
    implicit_oneway: true,
    implicit_oneway_confidence: "low",
    copy_surface_smoothness_from_parent: true,
    condition: |ctx| ctx.length_m <= 100.0 && is_crossing_pattern(ctx),
};

const BICYCLE_ROAD_VEHICLE_DESTINATION: BikelaneCategory = BikelaneCategory {
    id: "bicycleRoad_vehicleDestination",
    infrastructure_exists: true,
    implicit_oneway: false,
    implicit_oneway_confidence: "high",
    copy_surface_smoothness_from_parent: true,
    condition: |ctx| {
        if !is_bicycle_road(ctx) {
            return false;
        }
        let sign = tag(ctx, "traffic_sign");
        if sign_contains(sign, "1020-30")
            || sign_contains(sign, "Kraftfahrzeuge-frei")
            || sign_contains(sign, "Kfz-Verkehr frei")
            || sign_contains(sign, "KFZ frei")
        {
            return true;
        }
        tag_in(ctx, "vehicle", &["destination", "yes"])
            || tag_in(ctx, "motor_vehicle", &["destination", "yes"])
    },
};

const BICYCLE_ROAD: BikelaneCategory = BikelaneCategory {
    id: "bicycleRoad",
    infrastructure_exists: true,
    implicit_oneway: false,
    implicit_oneway_confidence: "high",
    copy_surface_smoothness_from_parent: true,
    condition: |ctx| is_bicycle_road(ctx),
};

const SHARED_BUS_LANE_BIKE_WITH_BUS: BikelaneCategory = BikelaneCategory {
    id: "sharedBusLaneBikeWithBus",
    infrastructure_exists: true,
    implicit_oneway: true,
    implicit_oneway_confidence: "high",
    copy_surface_smoothness_from_parent: true,
    condition: |ctx| {
        if is_crossing_pattern(ctx) || !tag_is(ctx, "highway", "cycleway") {
            return false;
        }
        let sign = tag(ctx, "traffic_sign");
        let parent_sign = ctx.parent_tags.and_then(|t| t.get("traffic_sign")).map(String::as_str);
        tag_is(ctx, "lane", "share_busway")
            || (sign_starts_with(sign, "DE:237")
                && (sign_contains(sign, "1024-14") || sign_contains(sign, "1026-32")))
            || (sign_starts_with(parent_sign, "DE:237")
                && (sign_contains(parent_sign, "1024-14") || sign_contains(parent_sign, "1026-32")))
    },
};

const SHARED_BUS_LANE_BUS_WITH_BIKE: BikelaneCategory = BikelaneCategory {
    id: "sharedBusLaneBusWithBike",
    infrastructure_exists: true,
    implicit_oneway: true,
    implicit_oneway_confidence: "high",
    copy_surface_smoothness_from_parent: true,
    condition: |ctx| {
        if is_crossing_pattern(ctx) || !tag_is(ctx, "highway", "cycleway") {
            return false;
        }
        let sign = tag(ctx, "traffic_sign");
        let parent_sign = ctx.parent_tags.and_then(|t| t.get("traffic_sign")).map(String::as_str);
        tag_in(ctx, "cycleway", &["share_busway", "opposite_share_busway"])
            || (sign_starts_with(sign, "DE:245")
                && (sign_contains(sign, "1022-10") || sign_contains(sign, "1022-14")))
            || (sign_starts_with(parent_sign, "DE:245")
                && (sign_contains(parent_sign, "1022-10") || sign_contains(parent_sign, "1022-14")))
    },
};

const PEDESTRIAN_AREA_BICYCLE_YES: BikelaneCategory = BikelaneCategory {
    id: "pedestrianAreaBicycleYes",
    infrastructure_exists: true,
    implicit_oneway: false,
    implicit_oneway_confidence: "high",
    copy_surface_smoothness_from_parent: false,
    condition: |ctx| {
        tag_is(ctx, "highway", "pedestrian") && tag_in(ctx, "bicycle", &["yes", "designated"])
    },
};

const SHARED_MOTOR_VEHICLE_LANE: BikelaneCategory = BikelaneCategory {
    id: "sharedMotorVehicleLane",
    infrastructure_exists: true,
    implicit_oneway: false,
    implicit_oneway_confidence: "high",
    copy_surface_smoothness_from_parent: true,
    condition: |ctx| {
        tag_is(ctx, "highway", "cycleway") && tag_is(ctx, "cycleway", "shared_lane")
    },
};

const CYCLEWAY_ON_HIGHWAY_BETWEEN_LANES: BikelaneCategory = BikelaneCategory {
    id: "cyclewayOnHighwayBetweenLanes",
    infrastructure_exists: true,
    implicit_oneway: true,
    implicit_oneway_confidence: "high",
    copy_surface_smoothness_from_parent: true,
    condition: |ctx| {
        if ctx.side != Side::Self_ {
            return false;
        }
        sign_contains(tag(ctx, "cycleway:lanes"), "|lane|")
            || sign_contains(tag(ctx, "bicycle:lanes"), "|designated|")
    },
};

// Subcategories generated by CreateSubcategoriesAdjoiningOrIsolated:

const FOOT_AND_CYCLEWAY_SHARED_ADJOINING: BikelaneCategory = BikelaneCategory {
    id: "footAndCyclewayShared_adjoining",
    infrastructure_exists: true,
    implicit_oneway: true,
    implicit_oneway_confidence: "low",
    copy_surface_smoothness_from_parent: false,
    condition: |ctx| {
        is_foot_and_cycleway_shared_base(ctx) && is_sidepath(ctx) && !tag_is(ctx, "is_sidepath", "no")
    },
};

const FOOT_AND_CYCLEWAY_SHARED_ISOLATED: BikelaneCategory = BikelaneCategory {
    id: "footAndCyclewayShared_isolated",
    infrastructure_exists: true,
    implicit_oneway: true,
    implicit_oneway_confidence: "low",
    copy_surface_smoothness_from_parent: false,
    condition: |ctx| {
        is_foot_and_cycleway_shared_base(ctx)
            && (tag_is(ctx, "is_sidepath", "no")
                || tag_in(ctx, "highway", &["service", "track"]))
    },
};

const FOOT_AND_CYCLEWAY_SHARED_ADJOINING_OR_ISOLATED: BikelaneCategory = BikelaneCategory {
    id: "footAndCyclewayShared_adjoiningOrIsolated",
    infrastructure_exists: true,
    implicit_oneway: true,
    implicit_oneway_confidence: "low",
    copy_surface_smoothness_from_parent: false,
    condition: |ctx| {
        is_foot_and_cycleway_shared_base(ctx)
            && !is_sidepath(ctx)
            && !tag_is(ctx, "is_sidepath", "no")
            && !tag_in(ctx, "highway", &["service", "track"])
    },
};

const FOOT_AND_CYCLEWAY_SEGREGATED_ADJOINING: BikelaneCategory = BikelaneCategory {
    id: "footAndCyclewaySegregated_adjoining",
    infrastructure_exists: true,
    implicit_oneway: true,
    implicit_oneway_confidence: "low",
    copy_surface_smoothness_from_parent: false,
    condition: |ctx| {
        is_foot_and_cycleway_segregated_base(ctx)
            && is_sidepath(ctx)
            && !tag_is(ctx, "is_sidepath", "no")
    },
};

const FOOT_AND_CYCLEWAY_SEGREGATED_ISOLATED: BikelaneCategory = BikelaneCategory {
    id: "footAndCyclewaySegregated_isolated",
    infrastructure_exists: true,
    implicit_oneway: true,
    implicit_oneway_confidence: "low",
    copy_surface_smoothness_from_parent: false,
    condition: |ctx| {
        is_foot_and_cycleway_segregated_base(ctx)
            && (tag_is(ctx, "is_sidepath", "no")
                || tag_in(ctx, "highway", &["service", "track"]))
    },
};

const FOOT_AND_CYCLEWAY_SEGREGATED_ADJOINING_OR_ISOLATED: BikelaneCategory = BikelaneCategory {
    id: "footAndCyclewaySegregated_adjoiningOrIsolated",
    infrastructure_exists: true,
    implicit_oneway: true,
    implicit_oneway_confidence: "low",
    copy_surface_smoothness_from_parent: false,
    condition: |ctx| {
        is_foot_and_cycleway_segregated_base(ctx)
            && !is_sidepath(ctx)
            && !tag_is(ctx, "is_sidepath", "no")
            && !tag_in(ctx, "highway", &["service", "track"])
    },
};

const CYCLEWAY_SEPARATED_ADJOINING: BikelaneCategory = BikelaneCategory {
    id: "cyclewaySeparated_adjoining",
    infrastructure_exists: true,
    implicit_oneway: false,
    implicit_oneway_confidence: "low",
    copy_surface_smoothness_from_parent: false,
    condition: |ctx| {
        is_cycleway_separated_base(ctx) && is_sidepath(ctx) && !tag_is(ctx, "is_sidepath", "no")
    },
};

const CYCLEWAY_SEPARATED_ISOLATED: BikelaneCategory = BikelaneCategory {
    id: "cyclewaySeparated_isolated",
    infrastructure_exists: true,
    implicit_oneway: false,
    implicit_oneway_confidence: "low",
    copy_surface_smoothness_from_parent: false,
    condition: |ctx| {
        is_cycleway_separated_base(ctx)
            && (tag_is(ctx, "is_sidepath", "no")
                || tag_in(ctx, "highway", &["service", "track"]))
    },
};

const CYCLEWAY_SEPARATED_ADJOINING_OR_ISOLATED: BikelaneCategory = BikelaneCategory {
    id: "cyclewaySeparated_adjoiningOrIsolated",
    infrastructure_exists: true,
    implicit_oneway: false,
    implicit_oneway_confidence: "low",
    copy_surface_smoothness_from_parent: false,
    condition: |ctx| {
        is_cycleway_separated_base(ctx)
            && !is_sidepath(ctx)
            && !tag_is(ctx, "is_sidepath", "no")
            && !tag_in(ctx, "highway", &["service", "track"])
    },
};

const CYCLEWAY_ON_HIGHWAY_ADVISORY: BikelaneCategory = BikelaneCategory {
    id: "cyclewayOnHighway_advisory",
    infrastructure_exists: true,
    implicit_oneway: true,
    implicit_oneway_confidence: "high",
    copy_surface_smoothness_from_parent: true,
    condition: |ctx| is_advisory_or_exclusive(ctx) && tag_is(ctx, "lane", "advisory"),
};

const CYCLEWAY_ON_HIGHWAY_EXCLUSIVE: BikelaneCategory = BikelaneCategory {
    id: "cyclewayOnHighway_exclusive",
    infrastructure_exists: true,
    implicit_oneway: true,
    implicit_oneway_confidence: "high",
    copy_surface_smoothness_from_parent: true,
    condition: |ctx| is_advisory_or_exclusive(ctx) && tag_is(ctx, "lane", "exclusive"),
};

const CYCLEWAY_ON_HIGHWAY_ADVISORY_OR_EXCLUSIVE: BikelaneCategory = BikelaneCategory {
    id: "cyclewayOnHighway_advisoryOrExclusive",
    infrastructure_exists: true,
    implicit_oneway: true,
    implicit_oneway_confidence: "medium",
    copy_surface_smoothness_from_parent: true,
    condition: |ctx| is_advisory_or_exclusive(ctx),
};

const FOOTWAY_BICYCLE_YES_ADJOINING: BikelaneCategory = BikelaneCategory {
    id: "footwayBicycleYes_adjoining",
    infrastructure_exists: true,
    implicit_oneway: true,
    implicit_oneway_confidence: "low",
    copy_surface_smoothness_from_parent: false,
    condition: |ctx| {
        is_footway_bicycle_yes_base(ctx) && is_sidepath(ctx) && !tag_is(ctx, "is_sidepath", "no")
    },
};

const FOOTWAY_BICYCLE_YES_ISOLATED: BikelaneCategory = BikelaneCategory {
    id: "footwayBicycleYes_isolated",
    infrastructure_exists: true,
    implicit_oneway: true,
    implicit_oneway_confidence: "low",
    copy_surface_smoothness_from_parent: false,
    condition: |ctx| {
        is_footway_bicycle_yes_base(ctx)
            && (tag_is(ctx, "is_sidepath", "no")
                || tag_in(ctx, "highway", &["service", "track"]))
    },
};

const FOOTWAY_BICYCLE_YES_ADJOINING_OR_ISOLATED: BikelaneCategory = BikelaneCategory {
    id: "footwayBicycleYes_adjoiningOrIsolated",
    infrastructure_exists: true,
    implicit_oneway: true,
    implicit_oneway_confidence: "low",
    copy_surface_smoothness_from_parent: false,
    condition: |ctx| {
        is_footway_bicycle_yes_base(ctx)
            && !is_sidepath(ctx)
            && !tag_is(ctx, "is_sidepath", "no")
            && !tag_in(ctx, "highway", &["service", "track"])
    },
};

const NEEDS_CLARIFICATION: BikelaneCategory = BikelaneCategory {
    id: "needsClarification",
    infrastructure_exists: true,
    implicit_oneway: false,
    implicit_oneway_confidence: "low",
    copy_surface_smoothness_from_parent: false,
    condition: |ctx| {
        // Skip if between-lanes conditions match (those are on self object)
        if sign_contains(tag(ctx, "cycleway:lanes"), "|lane|")
            || sign_contains(tag(ctx, "bicycle:lanes"), "|designated|")
        {
            return false;
        }
        if tag_is(ctx, "cycleway", "shared") {
            return false;
        }
        if tag_is(ctx, "highway", "cycleway") {
            return true;
        }
        if tag_is(ctx, "highway", "path") && tag_is(ctx, "bicycle", "designated") {
            if tag_is(ctx, "foot", "no") {
                let excluded_surfaces = &[
                    "ground", "dirt", "fine_gravel", "gravel", "pebblestone", "earth",
                ];
                if ctx.tags.keys().any(|k| k.starts_with("mtb:"))
                    || tag_is(ctx, "mtb", "yes")
                    || tag_in(ctx, "surface", excluded_surfaces)
                {
                    return false;
                }
                return true;
            }
            return true;
        }
        if tag_is(ctx, "highway", "footway") && tag_is(ctx, "bicycle", "designated") {
            return true;
        }
        false
    },
};

/// All categories in precedence order (first match wins).
/// Mirrors the `categoryDefinitions` table in BikelaneCategories.lua.
pub static CATEGORY_DEFINITIONS: &[&BikelaneCategory] = &[
    &DATA_NO,
    &SEPARATE_GEOMETRY,
    &NOT_EXPECTED,
    &CYCLEWAY_ON_HIGHWAY_PROTECTED,
    &CYCLEWAY_LINK,
    &CROSSING,
    &BICYCLE_ROAD_VEHICLE_DESTINATION,
    &BICYCLE_ROAD,
    &SHARED_BUS_LANE_BIKE_WITH_BUS,
    &SHARED_BUS_LANE_BUS_WITH_BIKE,
    &PEDESTRIAN_AREA_BICYCLE_YES,
    &SHARED_MOTOR_VEHICLE_LANE,
    &CYCLEWAY_ON_HIGHWAY_BETWEEN_LANES,
    &FOOT_AND_CYCLEWAY_SHARED_ADJOINING,
    &FOOT_AND_CYCLEWAY_SHARED_ISOLATED,
    &FOOT_AND_CYCLEWAY_SHARED_ADJOINING_OR_ISOLATED,
    &FOOT_AND_CYCLEWAY_SEGREGATED_ADJOINING,
    &FOOT_AND_CYCLEWAY_SEGREGATED_ISOLATED,
    &FOOT_AND_CYCLEWAY_SEGREGATED_ADJOINING_OR_ISOLATED,
    &CYCLEWAY_SEPARATED_ADJOINING,
    &CYCLEWAY_SEPARATED_ISOLATED,
    &CYCLEWAY_SEPARATED_ADJOINING_OR_ISOLATED,
    &CYCLEWAY_ON_HIGHWAY_ADVISORY,
    &CYCLEWAY_ON_HIGHWAY_EXCLUSIVE,
    &CYCLEWAY_ON_HIGHWAY_ADVISORY_OR_EXCLUSIVE,
    &FOOTWAY_BICYCLE_YES_ADJOINING,
    &FOOTWAY_BICYCLE_YES_ISOLATED,
    &FOOTWAY_BICYCLE_YES_ADJOINING_OR_ISOLATED,
    &NEEDS_CLARIFICATION,
];

/// Find the first matching category for the given context.
pub fn categorize_bikelane(ctx: &CategoryContext) -> Option<&'static BikelaneCategory> {
    CATEGORY_DEFINITIONS
        .iter()
        .copied()
        .find(|cat| (cat.condition)(ctx))
}
