import { useEffect, useRef } from "react";
import maplibregl from "maplibre-gl";

const SOURCE_ID = "live-editor-features";

export default function Map({
  bounds,
  data,
}: {
  bounds: [number, number, number, number] | null;
  data: GeoJSON.FeatureCollection | null;
}) {
  const containerRef = useRef<HTMLDivElement | null>(null);
  const mapRef = useRef<maplibregl.Map | null>(null);

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
    });
    mapRef.current = map;
    return () => {
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
