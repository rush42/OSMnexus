// Mirrors `geom::primitives::linestring_from_ewkb`/`mercator_to_wgs84` (`src/geom/primitives.rs`)
// byte-for-byte: `ST_AsEWKB` on an EPSG:3857 `geometry(LineString,3857)`/`geometry(Point,3857)`
// column round-trips exactly through these decoders, same as it does through the Rust one — see
// that module's own doc. Used only by `fetchFeatures`' binary-COPY path (`lib/liveEditor.ts`), which
// fetches raw EWKB instead of `ST_AsGeoJSON` text specifically to avoid paying for verbose ASCII
// coordinates on the wire.

const EARTH_RADIUS = 6_378_137.0;

function mercatorToWgs84(x: number, y: number): [number, number] {
  const lon = (x / EARTH_RADIUS) * (180 / Math.PI);
  const lat = (2 * Math.atan(Math.exp(y / EARTH_RADIUS)) - Math.PI / 2) * (180 / Math.PI);
  return [lon, lat];
}

// Decodes a little-endian, SRID-flagged EWKB LineString into WGS84 [lon,lat] coordinate pairs —
// the shape the live editor's way geometry always is.
export function linestringFromEwkb(buf: Buffer): [number, number][] {
  if (buf.length < 13) throw new Error("EWKB too short for a LineString header");
  if (buf[0] !== 1) throw new Error("only little-endian EWKB is supported");
  const wkbType = buf.readUInt32LE(1);
  if (wkbType !== 0x2000_0002) throw new Error(`expected SRID-flagged LineString, got type 0x${wkbType.toString(16)}`);
  // bytes[5..9] is the SRID, already known to be 3857 by construction.
  const numPoints = buf.readUInt32LE(9);
  if (buf.length < 13 + numPoints * 16) throw new Error("EWKB truncated");
  const coords: [number, number][] = [];
  for (let i = 0; i < numPoints; i++) {
    const off = 13 + i * 16;
    const x = buf.readDoubleLE(off);
    const y = buf.readDoubleLE(off + 8);
    coords.push(mercatorToWgs84(x, y));
  }
  return coords;
}

// Decodes a little-endian, SRID-flagged EWKB Point into a WGS84 [lon,lat] pair — the shape the live
// editor's node geometry always is. No count prefix, unlike LineString — a Point's header (1-byte
// endianness + 4-byte type + 4-byte SRID) is immediately followed by exactly one X/Y pair.
export function pointFromEwkb(buf: Buffer): [number, number] {
  if (buf.length < 25) throw new Error("EWKB too short for a Point");
  if (buf[0] !== 1) throw new Error("only little-endian EWKB is supported");
  const wkbType = buf.readUInt32LE(1);
  if (wkbType !== 0x2000_0001) throw new Error(`expected SRID-flagged Point, got type 0x${wkbType.toString(16)}`);
  // bytes[5..9] is the SRID, already known to be 3857 by construction.
  const x = buf.readDoubleLE(9);
  const y = buf.readDoubleLE(17);
  return mercatorToWgs84(x, y);
}
