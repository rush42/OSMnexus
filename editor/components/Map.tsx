"use client";

import { useEffect, useRef, useState } from "react";
import maplibregl from "maplibre-gl";

const SOURCE_ID = "live-editor-features";
const CUT_POINTS_SOURCE_ID = "live-editor-cut-points";
const DRAW_SOURCE_ID = "bbox-draw";
// Holds a single feature: a searched-for way the pipeline didn't classify into any category (see
// `unmatchedWayFeature`) — rendered gray, separately from the colored `SOURCE_ID` layers.
const SEARCH_FALLBACK_SOURCE_ID = "search-way-fallback";

const CUT_POINT_COLOR_EXPRESSION = [
  "match",
  ["get", "kind"],
  "cut",
  "#dc2626",
  "#000000",
] as unknown as maplibregl.ExpressionSpecification;

function boxToPolygon(a: [number, number], b: [number, number]): GeoJSON.Feature {
  const west = Math.min(a[0], b[0]);
  const east = Math.max(a[0], b[0]);
  const south = Math.min(a[1], b[1]);
  const north = Math.max(a[1], b[1]);
  return {
    type: "Feature",
    properties: {},
    geometry: {
      type: "Polygon",
      coordinates: [
        [
          [west, south],
          [east, south],
          [east, north],
          [west, north],
          [west, south],
        ],
      ],
    },
  };
}

// Builds a MapLibre `match` expression keyed on `topic` — one color per topic for now, categories
// within a topic aren't individually distinguished yet. Falls back to a neutral color for anything
// not yet in `topicColors`.
const FALLBACK_COLOR = "#e6432a";
function colorExpression(topicColors: Record<string, string>): maplibregl.ExpressionSpecification | string {
  const entries = Object.entries(topicColors);
  if (entries.length === 0) return FALLBACK_COLOR;
  return [
    "match",
    ["get", "topic"],
    ...entries.flatMap(([key, color]) => [key, color]),
    FALLBACK_COLOR,
  ] as unknown as maplibregl.ExpressionSpecification;
}

// A side-split object (`side_split.rs`) shares its parent way's centerline geometry — there's no
// offset geometry to compute server-side (see the `_side` field the pipeline stamps into `private`,
// exposed here via output/geojson.rs). Instead, nudge left/right lines apart purely at render time
// via MapLibre's `line-offset` (screen-space pixels, perpendicular to the line's own direction —
// positive is to the right of travel, per the MapLibre style spec). Scaled by zoom so the gap
// reads as roughly-constant on the ground instead of swamping the line at low zoom or vanishing at
// high zoom; `self` objects (undivided ways) get no offset.
// A zoom expression (`["zoom"]`) may only be used as a top-level expression, or as the direct input
// to a top-level "interpolate"/"step" — nesting an interpolate-on-zoom inside further arithmetic
// (a "*", or even a wrapping "match") makes MapLibre reject the whole layer at `addLayer` time
// ("zoom expression may only be used as input to a top-level interpolate/step expression"), which
// silently broke every other layer/filter/paint call targeting `${SOURCE_ID}-line` since the layer
// never existed. So `interpolate` has to be the outermost expression here; the left/right sign is
// instead baked into each stop's *output value*, which is allowed to be an arbitrary (data-driven)
// expression.
const SIDE_SIGN = ["match", ["get", "_side"], "left", -1, "right", 1, 0] as unknown as maplibregl.ExpressionSpecification;
const LINE_OFFSET_EXPRESSION = [
  "interpolate",
  ["linear"],
  ["zoom"],
  12,
  ["*", SIDE_SIGN, 0.5],
  20,
  ["*", SIDE_SIGN, 6],
] as unknown as maplibregl.ExpressionSpecification;

// Visits every [lon, lat] pair in a geometry's (possibly nested, for Polygon/MultiLineString/...)
// `coordinates` array — generic over geometry type since focusing a category only needs the
// extent, not the shape.
function forEachCoordinate(geometry: GeoJSON.Geometry, visit: (lon: number, lat: number) => void) {
  const walk = (node: unknown): void => {
    if (Array.isArray(node) && typeof node[0] === "number") {
      visit(node[0] as number, node[1] as number);
    } else if (Array.isArray(node)) {
      node.forEach(walk);
    }
  };
  if ("coordinates" in geometry) walk(geometry.coordinates);
}

// Derives point features at each line feature's endpoints (`LineString`/`MultiLineString` only) —
// the "Show intersections" fallback for topics that emit `"line"` geometry (whole, uncut ways)
// instead of `"graph"` (intersection-split segments, whose real cut points come from the backend
// via `cutPoints` — see `output/geojson.rs`). Not verified intersections, just where two ways'
// endpoints happen to land on the same coordinate — the best a `"line"`-only topic can show without
// the graph relationship, but it's what overlapping endpoints visually read as anyway.
function lineEndpoints(fc: GeoJSON.FeatureCollection): GeoJSON.FeatureCollection {
  const points: GeoJSON.Feature[] = [];
  for (const feature of fc.features) {
    const { geometry, properties } = feature;
    const lines =
      geometry.type === "LineString"
        ? [geometry.coordinates]
        : geometry.type === "MultiLineString"
          ? geometry.coordinates
          : [];
    for (const coords of lines) {
      if (coords.length < 2) continue;
      for (const c of [coords[0], coords[coords.length - 1]]) {
        points.push({
          type: "Feature",
          geometry: { type: "Point", coordinates: c },
          properties: { ...properties, kind: "endpoint" },
        });
      }
    }
  }
  return { type: "FeatureCollection", features: points };
}

// Excludes features whose `topic` property is in `hiddenTopics`; when `isolateCategory` is set
// (a category, as opposed to a topic's topic.json, is selected in the sidebar) also excludes every
// feature outside that single topic+category — "clicking a category hides all others" — using the
// `category` property the pipeline stamps onto `derived` from the matched category's file stem
// (see `engine::runner`), so no extra plumbing is needed to know which category a feature came from.
function visibilityFilter(
  hiddenTopics: Set<string>,
  isolateCategory: { topic: string; name: string } | null,
): maplibregl.ExpressionSpecification {
  const notHidden = ["!", ["in", ["get", "topic"], ["literal", [...hiddenTopics]]]];
  if (!isolateCategory) return notHidden as unknown as maplibregl.ExpressionSpecification;
  return [
    "all",
    notHidden,
    ["==", ["get", "topic"], isolateCategory.topic],
    ["==", ["get", "category"], isolateCategory.name],
  ] as unknown as maplibregl.ExpressionSpecification;
}

export default function Map({
  bounds,
  data,
  cutPoints,
  topicColors,
  hiddenTopics,
  isolateCategory,
  focusTarget,
  focusTick,
  followSelection,
  showNodes,
  highlightWayId,
  unmatchedWayFeature,
  onBboxSelected,
}: {
  bounds: [number, number, number, number] | null;
  data: GeoJSON.FeatureCollection | null;
  cutPoints: GeoJSON.FeatureCollection | null;
  topicColors: Record<string, string>;
  hiddenTopics: Set<string>;
  isolateCategory: { topic: string; name: string } | null;
  // What to fit the view to on the next focusTick — a topic click (name: null) fits every feature
  // in that topic, a category click (name set) narrows to just that category. Separate from
  // `isolateCategory` since focusing a topic shouldn't also hide its other categories on the map.
  focusTarget: { topic: string; name: string | null } | null;
  // Bumped on every category/topic click (even re-clicking the already-active one) so the
  // fit-to-selection effect below can distinguish "clicked again, please refocus" from an unrelated
  // re-render — a ref/content comparison on focusTarget alone can't tell those apart when it's the
  // same value.
  focusTick: number;
  // Gates the fit-to-selection effect — lets users click around the sidebar without the map
  // yanking the viewport away each time, when they'd rather navigate manually.
  followSelection: boolean;
  showNodes: boolean;
  // The osm_id found via the sidebar's way-id search, if any — drives the highlight layer below.
  // Every classified feature carries `properties.osm_id` (stamped in `src/output/geojson.rs`), so
  // no extra data plumbing is needed to find the match once `data` includes it.
  highlightWayId: string | null;
  // A searched-for way that came back unclassified from the pipeline (no category matched it) —
  // its raw geometry + OSM tags, rendered gray via SEARCH_FALLBACK_SOURCE_ID instead of the normal
  // colored feature layers, so a search always shows *something* even with no matching category.
  // `null` whenever the last search's way *was* classified (the normal highlight layer covers it).
  unmatchedWayFeature: GeoJSON.Feature | null;
  onBboxSelected: (bounds: [number, number, number, number]) => void;
}) {
  const containerRef = useRef<HTMLDivElement | null>(null);
  const mapRef = useRef<maplibregl.Map | null>(null);
  const onBboxSelectedRef = useRef(onBboxSelected);
  onBboxSelectedRef.current = onBboxSelected;
  const dataRef = useRef(data);
  dataRef.current = data;
  // Flips true exactly once, from the map's own "load" event — every other effect below depends on
  // it instead of each re-deriving "is the map ready" via `map.loaded()` + a one-off `map.once("load")`
  // listener. That ad-hoc pattern raced: if "load" had already fired by the time an effect ran (timing
  // depends on network/image load, not just React's render), `map.once("load", ...)` would attach to
  // an event that already happened and never fire — the "sometimes it just doesn't show up" bug.
  // Depending on `ready` state instead means React itself re-runs every effect when it flips, with no
  // manual event-listener race possible.
  const [ready, setReady] = useState(false);
  // The linestring the user last clicked on the map — distinct from `highlightWayId` (search-driven,
  // any geometry type): this is purely a click-to-highlight affordance, local to the map since
  // nothing outside it currently needs to know about it. Cleared on a click that misses every
  // feature layer.
  const [clickedWayId, setClickedWayId] = useState<number | null>(null);

  useEffect(() => {
    if (!containerRef.current || mapRef.current) return;
    const map = new maplibregl.Map({
      container: containerRef.current,
      // CARTO Positron: a clean, low-contrast vector basemap (no API key needed) so the
      // classified feature colors stay legible instead of competing with a busy raster OSM tile.
      style: "https://basemaps.cartocdn.com/gl/positron-gl-style/style.json",
      center: [13.3, 52.51],
      zoom: 13,
      boxZoom: false,
    });
    map.on("load", () => {
      map.addSource(SOURCE_ID, { type: "geojson", data: { type: "FeatureCollection", features: [] } });
      map.addLayer({
        id: `${SOURCE_ID}-line`,
        type: "line",
        source: SOURCE_ID,
        paint: { "line-color": FALLBACK_COLOR, "line-width": 3, "line-offset": LINE_OFFSET_EXPRESSION },
      });
      map.addLayer({
        id: `${SOURCE_ID}-point`,
        type: "circle",
        source: SOURCE_ID,
        filter: ["==", ["geometry-type"], "Point"],
        paint: { "circle-color": FALLBACK_COLOR, "circle-radius": 5 },
      });

      // Way-search highlight — a distinct outline drawn on top of the regular feature layers,
      // filtered to a single osm_id. Starts matching nothing (filter set in the highlightWayId
      // effect below); kept as an always-present layer rather than added/removed so its paint
      // config doesn't need to be duplicated at add-time.
      map.addLayer({
        id: `${SOURCE_ID}-highlight-line`,
        type: "line",
        source: SOURCE_ID,
        filter: ["==", ["get", "osm_id"], -1],
        paint: { "line-color": "#ffdd00", "line-width": 7, "line-opacity": 0.7 },
      });
      map.addLayer({
        id: `${SOURCE_ID}-highlight-point`,
        type: "circle",
        source: SOURCE_ID,
        filter: ["all", ["==", ["geometry-type"], "Point"], ["==", ["get", "osm_id"], -1]],
        paint: { "circle-color": "#ffdd00", "circle-radius": 9, "circle-opacity": 0.5 },
      });

      // Click highlight — a distinct outline for whichever linestring the user last clicked, same
      // filtered-overlay-layer pattern as the way-search highlight above but scoped to LineStrings
      // and kept visually distinct so the two selections don't collide.
      map.addLayer({
        id: `${SOURCE_ID}-click-highlight-line`,
        type: "line",
        source: SOURCE_ID,
        filter: ["all", ["==", ["geometry-type"], "LineString"], ["==", ["get", "osm_id"], -1]],
        paint: { "line-color": "#ff3388", "line-width": 6, "line-opacity": 0.8 },
      });

      map.addSource(SEARCH_FALLBACK_SOURCE_ID, { type: "geojson", data: { type: "FeatureCollection", features: [] } });
      map.addLayer({
        id: `${SEARCH_FALLBACK_SOURCE_ID}-line`,
        type: "line",
        source: SEARCH_FALLBACK_SOURCE_ID,
        paint: { "line-color": "#888888", "line-width": 4, "line-dasharray": [1, 1] },
      });
      map.addLayer({
        id: `${SEARCH_FALLBACK_SOURCE_ID}-point`,
        type: "circle",
        source: SEARCH_FALLBACK_SOURCE_ID,
        filter: ["==", ["geometry-type"], "Point"],
        paint: { "circle-color": "#888888", "circle-radius": 5 },
      });

      map.addSource(CUT_POINTS_SOURCE_ID, { type: "geojson", data: { type: "FeatureCollection", features: [] } });
      map.addLayer({
        id: `${CUT_POINTS_SOURCE_ID}-halo`,
        type: "circle",
        source: CUT_POINTS_SOURCE_ID,
        paint: { "circle-color": "#ffffff", "circle-radius": 5 },
      });
      map.addLayer({
        id: `${CUT_POINTS_SOURCE_ID}-point`,
        type: "circle",
        source: CUT_POINTS_SOURCE_ID,
        // `"kind": "cut"` (a graph-shape mid-way split, see `output/geojson.rs`) in red, everything
        // else — `"endpoint"` (a way's own two ends, real or derived — see `lineEndpoints`) — black.
        paint: {
          "circle-color": CUT_POINT_COLOR_EXPRESSION,
          "circle-radius": 3,
          "circle-stroke-color": "#ffffff",
          "circle-stroke-width": 1,
        },
      });

      const popup = new maplibregl.Popup({ closeButton: true, closeOnClick: true, maxWidth: "320px" });
      const featureLayers = [`${SOURCE_ID}-line`, `${SOURCE_ID}-point`, `${SEARCH_FALLBACK_SOURCE_ID}-line`, `${SEARCH_FALLBACK_SOURCE_ID}-point`];
      const showPopup = (e: maplibregl.MapLayerMouseEvent) => {
        const feature = e.features?.[0];
        if (!feature) return;
        setClickedWayId(feature.geometry.type === "LineString" ? Number(feature.properties?.osm_id) : null);
        const rows = Object.entries(feature.properties ?? {})
          .map(([k, v]) => `<tr><td style="color:#888;padding-right:8px;">${k}</td><td>${v}</td></tr>`)
          .join("");
        popup
          .setLngLat(e.lngLat)
          .setHTML(`<table style="font:12px monospace;border-collapse:collapse;">${rows}</table>`)
          .addTo(map);
      };
      map.on("click", featureLayers, showPopup);
      map.on("mouseenter", featureLayers, () => (map.getCanvas().style.cursor = "pointer"));
      map.on("mouseleave", featureLayers, () => (map.getCanvas().style.cursor = ""));
      // A click that misses every feature layer clears the click-highlight instead of leaving it
      // stuck on whatever was last clicked.
      map.on("click", (e: maplibregl.MapMouseEvent) => {
        if (map.queryRenderedFeatures(e.point, { layers: featureLayers }).length === 0) setClickedWayId(null);
      });

      map.addSource(DRAW_SOURCE_ID, { type: "geojson", data: { type: "FeatureCollection", features: [] } });
      map.addLayer({
        id: `${DRAW_SOURCE_ID}-fill`,
        type: "fill",
        source: DRAW_SOURCE_ID,
        paint: { "fill-color": "#2a7be6", "fill-opacity": 0.15 },
      });
      map.addLayer({
        id: `${DRAW_SOURCE_ID}-line`,
        type: "line",
        source: DRAW_SOURCE_ID,
        paint: { "line-color": "#2a7be6", "line-width": 2, "line-dasharray": [2, 2] },
      });

      setReady(true);
    });

    let start: [number, number] | null = null;
    const drawSource = () => map.getSource(DRAW_SOURCE_ID) as maplibregl.GeoJSONSource | undefined;

    const onMouseDown = (e: maplibregl.MapMouseEvent) => {
      if (!e.originalEvent.shiftKey) return;
      e.preventDefault();
      start = [e.lngLat.lng, e.lngLat.lat];
      map.dragPan.disable();
      map.on("mousemove", onMouseMove);
      map.once("mouseup", onMouseUp);
    };
    const onMouseMove = (e: maplibregl.MapMouseEvent) => {
      if (!start) return;
      drawSource()?.setData({ type: "FeatureCollection", features: [boxToPolygon(start, [e.lngLat.lng, e.lngLat.lat])] });
    };
    const onMouseUp = (e: maplibregl.MapMouseEvent) => {
      map.off("mousemove", onMouseMove);
      map.dragPan.enable();
      if (!start) return;
      const end: [number, number] = [e.lngLat.lng, e.lngLat.lat];
      const bbox: [number, number, number, number] = [
        Math.min(start[0], end[0]),
        Math.min(start[1], end[1]),
        Math.max(start[0], end[0]),
        Math.max(start[1], end[1]),
      ];
      start = null;
      if (Math.abs(bbox[2] - bbox[0]) > 1e-6 && Math.abs(bbox[3] - bbox[1]) > 1e-6) {
        onBboxSelectedRef.current(bbox);
      }
    };
    map.on("mousedown", onMouseDown);

    mapRef.current = map;
    return () => {
      map.off("mousedown", onMouseDown);
      map.remove();
      mapRef.current = null;
      setReady(false);
    };
  }, []);

  useEffect(() => {
    if (!ready || !bounds) return;
    mapRef.current!.fitBounds([[bounds[0], bounds[1]], [bounds[2], bounds[3]]], { padding: 20, duration: 0 });
  }, [ready, bounds]);

  useEffect(() => {
    if (!ready || !data) return;
    (mapRef.current!.getSource(SOURCE_ID) as maplibregl.GeoJSONSource).setData(data);
  }, [ready, data]);

  useEffect(() => {
    if (!ready) return;
    const fc: GeoJSON.FeatureCollection = { type: "FeatureCollection", features: unmatchedWayFeature ? [unmatchedWayFeature] : [] };
    (mapRef.current!.getSource(SEARCH_FALLBACK_SOURCE_ID) as maplibregl.GeoJSONSource).setData(fc);
  }, [ready, unmatchedWayFeature]);

  // Fits the view to the selected topic's or category's own features on every click (including
  // re-clicking the already-active one — see `focusTick`'s doc comment). Reads `data` via a ref
  // rather than a dependency so it doesn't refit on every unrelated data refresh (e.g. re-classify
  // while typing), only on an actual click.
  useEffect(() => {
    if (!ready || !followSelection || !focusTarget) return;
    const fc = dataRef.current;
    if (!fc) return;
    const bounds = new maplibregl.LngLatBounds();
    for (const feature of fc.features) {
      if (feature.properties?.topic !== focusTarget.topic) continue;
      if (focusTarget.name !== null && feature.properties?.category !== focusTarget.name) continue;
      forEachCoordinate(feature.geometry, (lon, lat) => bounds.extend([lon, lat]));
    }
    if (!bounds.isEmpty()) mapRef.current!.fitBounds(bounds, { padding: 40, maxZoom: 18, duration: 300 });
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [ready, followSelection, focusTarget, focusTick]);

  // Fits the view to the searched-for way once it shows up in `data` (classified case) or via
  // `unmatchedWayFeature` (unclassified case), and updates the highlight layers' filter. Reads
  // `data` via the ref (see the focusTarget effect above) so it only refits right after a search,
  // not on every unrelated data refresh.
  useEffect(() => {
    if (!ready) return;
    const wayOsmId = highlightWayId != null ? Number(highlightWayId) : -1;
    const highlightFilter = ["==", ["get", "osm_id"], wayOsmId] as unknown as maplibregl.ExpressionSpecification;
    const highlightPointFilter = ["all", ["==", ["geometry-type"], "Point"], highlightFilter] as unknown as maplibregl.ExpressionSpecification;
    mapRef.current!.setFilter(`${SOURCE_ID}-highlight-line`, highlightFilter);
    mapRef.current!.setFilter(`${SOURCE_ID}-highlight-point`, highlightPointFilter);
    if (highlightWayId == null) return;
    const bounds = new maplibregl.LngLatBounds();
    const fc = dataRef.current;
    let found = false;
    if (fc) {
      for (const feature of fc.features) {
        if (feature.properties?.osm_id !== wayOsmId) continue;
        found = true;
        forEachCoordinate(feature.geometry, (lon, lat) => bounds.extend([lon, lat]));
      }
    }
    if (!found && unmatchedWayFeature) {
      forEachCoordinate(unmatchedWayFeature.geometry, (lon, lat) => bounds.extend([lon, lat]));
    }
    if (!bounds.isEmpty()) mapRef.current!.fitBounds(bounds, { padding: 60, maxZoom: 18, duration: 300 });
  }, [ready, highlightWayId, data, unmatchedWayFeature]);

  useEffect(() => {
    if (!ready) return;
    const wayOsmId = clickedWayId ?? -1;
    const filter = ["all", ["==", ["geometry-type"], "LineString"], ["==", ["get", "osm_id"], wayOsmId]] as unknown as maplibregl.ExpressionSpecification;
    mapRef.current!.setFilter(`${SOURCE_ID}-click-highlight-line`, filter);
  }, [ready, clickedWayId]);

  useEffect(() => {
    if (!ready) return;
    const expr = colorExpression(topicColors);
    mapRef.current!.setPaintProperty(`${SOURCE_ID}-line`, "line-color", expr);
    mapRef.current!.setPaintProperty(`${SOURCE_ID}-point`, "circle-color", expr);
  }, [ready, topicColors]);

  useEffect(() => {
    if (!ready) return;
    // Real graph cut points (topics with `"graph"` geometry) take priority; falls back to
    // derived line endpoints (see `lineEndpoints`) when the backend gave us none, e.g. every
    // visible topic only declared `"line"`.
    const fc = cutPoints?.features.length
      ? cutPoints
      : data
        ? lineEndpoints(data)
        : { type: "FeatureCollection" as const, features: [] };
    (mapRef.current!.getSource(CUT_POINTS_SOURCE_ID) as maplibregl.GeoJSONSource).setData(fc);
  }, [ready, cutPoints, data]);

  useEffect(() => {
    if (!ready) return;
    const visibility = showNodes ? "visible" : "none";
    mapRef.current!.setLayoutProperty(`${CUT_POINTS_SOURCE_ID}-halo`, "visibility", visibility);
    mapRef.current!.setLayoutProperty(`${CUT_POINTS_SOURCE_ID}-point`, "visibility", visibility);
  }, [ready, showNodes]);

  useEffect(() => {
    if (!ready) return;
    const map = mapRef.current!;
    const lineFilter = visibilityFilter(hiddenTopics, isolateCategory);
    const pointFilter = ["all", ["==", ["geometry-type"], "Point"], lineFilter] as unknown as maplibregl.ExpressionSpecification;
    map.setFilter(`${SOURCE_ID}-line`, lineFilter);
    map.setFilter(`${SOURCE_ID}-point`, pointFilter);
    // Cut points don't carry a `category` property (they're not a classified feature themselves —
    // see output/geojson.rs), so they stay topic-scoped only; isolating a category would otherwise
    // just hide every cut point outright.
    const cutPointFilter = visibilityFilter(hiddenTopics, null);
    map.setFilter(`${CUT_POINTS_SOURCE_ID}-halo`, cutPointFilter);
    map.setFilter(`${CUT_POINTS_SOURCE_ID}-point`, cutPointFilter);
  }, [ready, hiddenTopics, isolateCategory]);

  return <div ref={containerRef} style={{ width: "100%", height: "100%" }} />;
}
