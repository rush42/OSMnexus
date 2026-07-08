import { useEffect, useRef, useState } from "react";
import maplibregl from "maplibre-gl";

const SOURCE_ID = "live-editor-features";
const CUT_POINTS_SOURCE_ID = "live-editor-cut-points";
const DRAW_SOURCE_ID = "bbox-draw";

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

// Excludes features whose `topic` property is in `hiddenTopics`.
function visibilityFilter(hiddenTopics: Set<string>): maplibregl.ExpressionSpecification {
  return ["!", ["in", ["get", "topic"], ["literal", [...hiddenTopics]]]] as unknown as maplibregl.ExpressionSpecification;
}

export default function Map({
  bounds,
  data,
  cutPoints,
  topicColors,
  hiddenTopics,
  showNodes,
  onBboxSelected,
}: {
  bounds: [number, number, number, number] | null;
  data: GeoJSON.FeatureCollection | null;
  cutPoints: GeoJSON.FeatureCollection | null;
  topicColors: Record<string, string>;
  hiddenTopics: Set<string>;
  showNodes: boolean;
  onBboxSelected: (bounds: [number, number, number, number]) => void;
}) {
  const containerRef = useRef<HTMLDivElement | null>(null);
  const mapRef = useRef<maplibregl.Map | null>(null);
  const onBboxSelectedRef = useRef(onBboxSelected);
  onBboxSelectedRef.current = onBboxSelected;
  // Flips true exactly once, from the map's own "load" event — every other effect below depends on
  // it instead of each re-deriving "is the map ready" via `map.loaded()` + a one-off `map.once("load")`
  // listener. That ad-hoc pattern raced: if "load" had already fired by the time an effect ran (timing
  // depends on network/image load, not just React's render), `map.once("load", ...)` would attach to
  // an event that already happened and never fire — the "sometimes it just doesn't show up" bug.
  // Depending on `ready` state instead means React itself re-runs every effect when it flips, with no
  // manual event-listener race possible.
  const [ready, setReady] = useState(false);

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
        paint: { "line-color": FALLBACK_COLOR, "line-width": 3 },
      });
      map.addLayer({
        id: `${SOURCE_ID}-point`,
        type: "circle",
        source: SOURCE_ID,
        filter: ["==", ["geometry-type"], "Point"],
        paint: { "circle-color": FALLBACK_COLOR, "circle-radius": 5 },
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
        paint: { "circle-color": "#000000", "circle-radius": 3, "circle-stroke-color": "#ffffff", "circle-stroke-width": 1 },
      });

      const popup = new maplibregl.Popup({ closeButton: true, closeOnClick: true, maxWidth: "320px" });
      const featureLayers = [`${SOURCE_ID}-line`, `${SOURCE_ID}-point`];
      const showPopup = (e: maplibregl.MapLayerMouseEvent) => {
        const feature = e.features?.[0];
        if (!feature) return;
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
    const expr = colorExpression(topicColors);
    mapRef.current!.setPaintProperty(`${SOURCE_ID}-line`, "line-color", expr);
    mapRef.current!.setPaintProperty(`${SOURCE_ID}-point`, "circle-color", expr);
  }, [ready, topicColors]);

  useEffect(() => {
    if (!ready) return;
    const fc = cutPoints ?? { type: "FeatureCollection" as const, features: [] };
    (mapRef.current!.getSource(CUT_POINTS_SOURCE_ID) as maplibregl.GeoJSONSource).setData(fc);
  }, [ready, cutPoints]);

  useEffect(() => {
    if (!ready) return;
    const visibility = showNodes ? "visible" : "none";
    mapRef.current!.setLayoutProperty(`${CUT_POINTS_SOURCE_ID}-halo`, "visibility", visibility);
    mapRef.current!.setLayoutProperty(`${CUT_POINTS_SOURCE_ID}-point`, "visibility", visibility);
  }, [ready, showNodes]);

  useEffect(() => {
    if (!ready) return;
    const map = mapRef.current!;
    const lineFilter = visibilityFilter(hiddenTopics);
    const pointFilter = ["all", ["==", ["geometry-type"], "Point"], lineFilter] as unknown as maplibregl.ExpressionSpecification;
    map.setFilter(`${SOURCE_ID}-line`, lineFilter);
    map.setFilter(`${SOURCE_ID}-point`, pointFilter);
    map.setFilter(`${CUT_POINTS_SOURCE_ID}-halo`, lineFilter);
    map.setFilter(`${CUT_POINTS_SOURCE_ID}-point`, lineFilter);
  }, [ready, hiddenTopics]);

  return <div ref={containerRef} style={{ width: "100%", height: "100%" }} />;
}
