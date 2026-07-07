import { useEffect, useRef } from "react";
import maplibregl from "maplibre-gl";

const SOURCE_ID = "live-editor-features";
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

export default function Map({
  bounds,
  data,
  onBboxSelected,
}: {
  bounds: [number, number, number, number] | null;
  data: GeoJSON.FeatureCollection | null;
  onBboxSelected: (bounds: [number, number, number, number]) => void;
}) {
  const containerRef = useRef<HTMLDivElement | null>(null);
  const mapRef = useRef<maplibregl.Map | null>(null);
  const onBboxSelectedRef = useRef(onBboxSelected);
  onBboxSelectedRef.current = onBboxSelected;

  useEffect(() => {
    if (!containerRef.current || mapRef.current) return;
    const map = new maplibregl.Map({
      container: containerRef.current,
      style: {
        version: 8,
        sources: {
          osm: {
            type: "raster",
            tiles: ["https://tile.openstreetmap.org/{z}/{x}/{y}.png"],
            tileSize: 256,
            attribution: "&copy; OpenStreetMap contributors",
          },
        },
        layers: [{ id: "osm", type: "raster", source: "osm" }],
      },
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
        paint: { "line-color": "#e6432a", "line-width": 3 },
      });
      map.addLayer({
        id: `${SOURCE_ID}-point`,
        type: "circle",
        source: SOURCE_ID,
        filter: ["==", ["geometry-type"], "Point"],
        paint: { "circle-color": "#e6432a", "circle-radius": 5 },
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
    };
  }, []);

  useEffect(() => {
    const map = mapRef.current;
    if (!map || !bounds) return;
    const apply = () => map.fitBounds([[bounds[0], bounds[1]], [bounds[2], bounds[3]]], { padding: 20, duration: 0 });
    if (map.loaded()) apply();
    else map.once("load", apply);
  }, [bounds]);

  useEffect(() => {
    const map = mapRef.current;
    if (!map || !data) return;
    const setData = () => (map.getSource(SOURCE_ID) as maplibregl.GeoJSONSource)?.setData(data);
    if (map.loaded() && map.getSource(SOURCE_ID)) setData();
    else map.once("load", setData);
  }, [data]);

  return <div ref={containerRef} style={{ width: "100%", height: "100%" }} />;
}
