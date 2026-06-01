# Topic Engine — Open Complications

Topics that can't be ported as-is. Each section names the blocking issue and what would need to change in the engine.

---

## barriers — partial port (waterway + railway lines missing)

**Ported:** `barrierLines` for motorway/trunk highway lines.

**Not ported:** waterway lines (`waterway=river|canal`) and railway lines
(`railway=rail|light_rail` with `usage=main|branch`). These don't have a `highway` tag
so `read_highway_ways` never reads them and `should_exclude` would reject them anyway.

**Area barriers** (`natural=water` large polygons, `aeroway=aerodrome`) are a separate
table (`barrierAreas`) with Polygon geometry — also not ported.

**What's needed:**
- A generic element reader that can filter by any tag (not just `highway`)
- Polygon geometry support in the engine (for areas) and a "closed way → polygon" mode
- The topic spec needs an `"element_filter"` field: `{"tag": "waterway", "in": ["river", "canal"]}`

---

## bicycleParking — requires node reader + point geometry

**Lua:** processes nodes as points, closed ways as polygon centroids, open ways as line centroids.
Two output tables: `bicycleParking_points` and `bicycleParking_areas`.

**Blocking issues:**
1. Engine only reads highway ways. Nodes need a separate PBF reader pass.
2. Engine outputs `geometry(LineString)`. Point and Polygon outputs not supported.
3. Two output tables from one topic spec — engine currently assumes 1 table per topic.
4. `capacity_normalization` Lua function (parses `capacity` tag, normalises cargo bike capacity)
   would need a new Rust sanitizer.

**What's needed:**
- `"element_type": "node"` in TopicSpec
- Node reader pass in `osm/reader.rs` (collect nodes by tag filter, resolve coordinate)
- `"geometry": "point"` in TopicSpec (schema uses `geometry(Point, 3857)`)
- Multi-table topic support

---

## bikeroutes — requires relation reader + multilinestring geometry

**Lua:** processes `type=route, route=bicycle` relations as MultiLineString.

**Blocking issues:**
1. Engine doesn't read relations at all.
2. MultiLineString geometry not supported.
3. Relation geometry requires resolving member ways' coordinates (complex).

**What's needed:**
- Relation reader (two-pass: collect member way IDs, resolve geometries, assemble multilinestring)
- `"geometry": "multilinestring"` in TopicSpec

**Sanitizers needed (already implementable in Rust):**
- `network`: whitelist `lcn|rcn|ncn|icn`
- `cycle_highway`: whitelist `yes`
- `roundtrip`: whitelist `yes|no`
- `network_type`: whitelist `node_network|basic_network`
- `distance`: `tonumber()` → `parse_length` equivalent
- `colours`: replace `;` with `/` in colour tag

---

## boundaries — requires relation reader + multipolygon geometry

**Lua:** processes `type=boundary` relations as MultiPolygon + centroid point label.
Two output tables.

**Blocking issues:**
1. Relations not read.
2. MultiPolygon geometry not supported.
3. Two output tables.

---

## landuse — requires polygon geometry (closed ways + relations)

**Lua:** processes closed ways as polygons, multipolygon relations.
Filter: `landuse` or `amenity` in a short allowed list.

**Blocking issues:**
1. Engine processes all ways as linestrings. Closed ways need to be detected
   (`object.is_closed`) and output as polygons.
2. `"geometry": "polygon"` not supported.
3. No `highway` tag on landuse ways — `should_exclude` would reject them.

**What's needed:**
- `"element_filter"` in TopicSpec (filter by non-highway tags)
- `"geometry": "polygon"` for closed ways
- `is_closed` detection in the engine

---

## parking — too complex for current engine

**Lua:** a large multi-module system (`parking_separate_parking_areas`,
`parking_obstacle_areas`, `parking_crossing_points`, `parking_roads`, etc.) writing to
10+ tables including a node-road-mapping table. Geometry includes points, lines, polygons.

**Status:** Requires significant engine extensions (node reader, polygon, multi-table,
specialized derived fields like road-node proximity). Not a priority to port via JSON.
May be better implemented as dedicated Rust code outside the topic engine.

---

## places — requires node reader + point geometry

**Lua:** processes nodes (city, town, village, etc.) as points.
Filter: `place` in allowed list.

**Blocking issues:**
1. Nodes not read.
2. Point geometry not supported.
3. `population` tag parsed as number → new sanitizer needed.

**What's needed:** node reader, point geometry, `parse_number` sanitizer.

---

## poiClassification — requires node reader + complex category logic

**Lua:** processes nodes (and closed ways) with `shop=*`, `amenity=*`, `tourism=*`, `leisure=*`.
Uses `ShoppingAllowedListWithCategories` (a large hardcoded list with category assignments).

**Blocking issues:**
1. Nodes not read.
2. Category assignment comes from a Lua table (`ShoppingAllowedListWithCategories`) that maps
   tag values to category strings. This is essentially its own category system.
   Could be ported to JSON categories with `tag: "shop", exists: true` or per-value conditions,
   but the list is long (~50+ categories from `ShoppingAllowedListWithCategories.lua`).
3. `InferAddress` helper (copies address tags) — would need a new `copy_address_tags` sanitizer.

**What's needed:** node reader, point geometry. Category files would be straightforward
but numerous (one per POI category). InferAddress → new sanitizer.

---

## publicTransport — requires node reader + point geometry

**Lua:** processes nodes and closed ways (as centroid points).
Categories: `railway_station`, `ferry_station`, `subway_station`, `light_rail_station`, `tram_station`.

**Blocking issues:**
1. Nodes not read.
2. Point geometry not supported.
3. Tags `network` and `network:short` are copied with `osm_` prefix in Lua
   (raw-tagged convention). In our engine `osm` column is raw, `sanitized` is processed —
   the `osm_` prefix convention doesn't apply.

**What's needed:** node reader, point geometry. The categories are simple and could be
ported immediately once the engine supports nodes.

**Sanitizers needed:** none — just raw tag copy.

---

## trafficSigns — requires node reader + complex direction geometry

**Lua:** processes nodes with `traffic_sign=*`. Computes sign orientation from the way
the node belongs to (via a separate `_trafficSignDirections` helper table and SQL post-processing).

**Blocking issues:**
1. Nodes not read.
2. Direction angle computation requires knowing which way the node sits on — needs a
   node-to-way join that the current PBF reader doesn't build.
3. `splitDirections` logic (forward/backward sign variants) — complex, needs custom handling.

**Status:** This topic has significant post-processing logic that likely can't be expressed
as a topic spec. Consider keeping as dedicated Rust code.

---

## Common engine extensions to unlock most blocked topics

**Priority 1 (unlocks bicycleParking, places, poiClassification, publicTransport):**
- Node reader in `osm/reader.rs`: `read_nodes_by_filter(filter: &Filter) -> Vec<OsmNode>`
- `"geometry": "point"` in TopicSpec
- `"element_type": "node"` in TopicSpec
- DB schema: `geometry(Point, 3857)` variant

**Priority 2 (unlocks landuse, partial barriers):**
- `"element_filter"` in TopicSpec (filter by non-highway tags, bypassing `should_exclude`)
- `"geometry": "polygon"` + `is_closed` detection
- `"element_type": "way_closed"` in TopicSpec

**Priority 3 (unlocks bikeroutes, boundaries):**
- Relation reader with multilinestring/multipolygon assembly
- `"element_type": "relation"` in TopicSpec
