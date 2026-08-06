use geo::{Centroid, Length, LineString, Polygon, Haversine};

const R: f64 = 6_378_137.0;

pub fn wgs84_to_3857(lon_deg: f64, lat_deg: f64) -> (f64, f64) {
    let x = R * lon_deg.to_radians();
    let y = R * (std::f64::consts::FRAC_PI_4 + lat_deg.to_radians() / 2.0).tan().ln();
    (x, y)
}

/// Project a WGS84 linestring to EPSG:3857 (Web Mercator).
pub fn project_line(coords: &[(f64, f64)]) -> LineString<f64> {
    coords
        .iter()
        .map(|&(lon, lat)| {
            let (x, y) = wgs84_to_3857(lon, lat);
            geo::coord! { x: x, y: y }
        })
        .collect()
}

/// Length in metres on the WGS84 ellipsoid (Haversine approximation).
pub fn haversine_length_m(coords: &[(f64, f64)]) -> f64 {
    if coords.len() < 2 {
        return 0.0;
    }
    let ls: LineString<f64> = coords
        .iter()
        .map(|&(lon, lat)| geo::coord! { x: lon, y: lat })
        .collect();
    ls.length::<Haversine>()
}

/// Centroid of an already-projected line (its vertices' centroid — the "line" reading of
/// `geo::Centroid`, not an area-weighted polygon centroid). `None` only for a degenerate
/// (empty) line, which never occurs for a resolved way (`resolve_geometry` requires ≥2 points).
pub fn centroid_of_line(geom: &LineString<f64>) -> Option<(f64, f64)> {
    geom.centroid().map(|p| (p.x(), p.y()))
}

/// Close a WGS84 coordinate ring (repeat the first point at the end if not already closed) and
/// project it to EPSG:3857 as a single-ring `Polygon` — the `way` reading of `Polygon` (a closed
/// way, e.g. a building or area, with no inner rings).
pub fn project_ring(coords: &[(f64, f64)]) -> Polygon<f64> {
    let mut ring = coords.to_vec();
    if ring.first() != ring.last() {
        ring.push(ring[0]);
    }
    Polygon::new(project_line(&ring), vec![])
}

/// Close + project an exterior ring plus zero or more interior (hole) rings — the `relation`
/// reading of `Polygon` (multipolygon assembly from member `outer`/`inner` ways, see
/// `osm::relation_geometry`). Each ring is independently closed the same way `project_ring` closes
/// a single one.
pub fn project_polygon(exterior: &[(f64, f64)], interiors: &[Vec<(f64, f64)>]) -> Polygon<f64> {
    let close = |coords: &[(f64, f64)]| -> LineString<f64> {
        let mut ring = coords.to_vec();
        if ring.first() != ring.last() {
            ring.push(ring[0]);
        }
        project_line(&ring)
    };
    Polygon::new(close(exterior), interiors.iter().map(|r| close(r)).collect())
}

/// Inverse of `wgs84_to_3857`.
pub fn mercator_to_wgs84(x: f64, y: f64) -> (f64, f64) {
    let lon = (x / R).to_degrees();
    let lat = (2.0 * (y / R).exp().atan() - std::f64::consts::FRAC_PI_2).to_degrees();
    (lon, lat)
}

/// Decode a LineString written by `to_ewkb` (little-endian, SRID-flagged) back into its raw
/// (still-projected) coordinates. Only the LineString shape is supported since that's the only
/// geometry type the edge/way-geometry writers ever produce.
pub fn linestring_from_ewkb(bytes: &[u8]) -> anyhow::Result<Vec<(f64, f64)>> {
    anyhow::ensure!(bytes.len() >= 13, "EWKB too short for a LineString header");
    anyhow::ensure!(bytes[0] == 1, "only little-endian EWKB is supported");
    let wkb_type = u32::from_le_bytes(bytes[1..5].try_into().unwrap());
    anyhow::ensure!(wkb_type == 0x2000_0002, "expected SRID-flagged LineString, got type {wkb_type:#x}");
    // bytes[5..9] is the SRID, already known to be 3857 by construction.
    let num_points = u32::from_le_bytes(bytes[9..13].try_into().unwrap()) as usize;
    anyhow::ensure!(bytes.len() >= 13 + num_points * 16, "EWKB truncated");
    let mut coords = Vec::with_capacity(num_points);
    for i in 0..num_points {
        let off = 13 + i * 16;
        let x = f64::from_le_bytes(bytes[off..off + 8].try_into().unwrap());
        let y = f64::from_le_bytes(bytes[off + 8..off + 16].try_into().unwrap());
        coords.push((x, y));
    }
    Ok(coords)
}

/// Decode a Point written by `point_to_ewkb` back into its raw (still-projected) coordinates.
pub fn point_from_ewkb(bytes: &[u8]) -> anyhow::Result<(f64, f64)> {
    anyhow::ensure!(bytes.len() >= 21, "EWKB too short for a Point");
    anyhow::ensure!(bytes[0] == 1, "only little-endian EWKB is supported");
    let wkb_type = u32::from_le_bytes(bytes[1..5].try_into().unwrap());
    anyhow::ensure!(wkb_type == 0x2000_0001, "expected SRID-flagged Point, got type {wkb_type:#x}");
    // bytes[5..9] is the SRID, already known to be 3857 by construction.
    let x = f64::from_le_bytes(bytes[9..17].try_into().unwrap());
    let y = f64::from_le_bytes(bytes[17..25].try_into().unwrap());
    Ok((x, y))
}

/// Encode a projected (EPSG:3857) Point as PostGIS EWKB with SRID.
pub fn point_to_ewkb(x: f64, y: f64) -> Vec<u8> {
    use std::io::Write;

    // WKB type for Point with SRID flag: 0x20000001
    let wkb_type: u32 = 0x2000_0001;
    let srid: i32 = 3857;

    let mut buf: Vec<u8> = Vec::with_capacity(21);
    buf.write_all(&[1u8]).unwrap();
    buf.write_all(&wkb_type.to_le_bytes()).unwrap();
    buf.write_all(&srid.to_le_bytes()).unwrap();
    buf.write_all(&x.to_le_bytes()).unwrap();
    buf.write_all(&y.to_le_bytes()).unwrap();
    buf
}

/// Encode a projected (EPSG:3857) Polygon — exterior ring plus any interior (hole) rings — as
/// PostGIS EWKB with SRID. A single-ring `Polygon` (e.g. from `project_ring`) is just the
/// zero-interior-rings case.
pub fn polygon_to_ewkb(polygon: &Polygon<f64>) -> Vec<u8> {
    use std::io::Write;

    let rings: Vec<Vec<_>> = std::iter::once(polygon.exterior())
        .chain(polygon.interiors())
        .map(|r| r.coords().collect())
        .collect();
    let num_rings = rings.len() as u32;

    // WKB type for Polygon with SRID flag: 0x20000003
    let wkb_type: u32 = 0x2000_0003;
    let srid: i32 = 3857;

    let total_points: usize = rings.iter().map(Vec::len).sum();
    let mut buf: Vec<u8> = Vec::with_capacity(13 + 4 * rings.len() + 16 * total_points);
    buf.write_all(&[1u8]).unwrap();
    buf.write_all(&wkb_type.to_le_bytes()).unwrap();
    buf.write_all(&srid.to_le_bytes()).unwrap();
    buf.write_all(&num_rings.to_le_bytes()).unwrap();
    for ring in &rings {
        buf.write_all(&(ring.len() as u32).to_le_bytes()).unwrap();
        for c in ring {
            buf.write_all(&c.x.to_le_bytes()).unwrap();
            buf.write_all(&c.y.to_le_bytes()).unwrap();
        }
    }
    buf
}

/// Decode a MultiLineString written by `to_multi_ewkb` back into its raw (still-projected)
/// per-run coordinate lists.
pub fn multilinestring_from_ewkb(bytes: &[u8]) -> anyhow::Result<Vec<Vec<(f64, f64)>>> {
    anyhow::ensure!(bytes.len() >= 9, "EWKB too short for a MultiLineString header");
    anyhow::ensure!(bytes[0] == 1, "only little-endian EWKB is supported");
    let wkb_type = u32::from_le_bytes(bytes[1..5].try_into().unwrap());
    anyhow::ensure!(wkb_type == 0x2000_0005, "expected SRID-flagged MultiLineString, got type {wkb_type:#x}");
    // bytes[5..9] is the SRID, already known to be 3857 by construction.
    let num_lines = u32::from_le_bytes(bytes[9..13].try_into().unwrap()) as usize;
    let mut pos = 13;
    let mut lines = Vec::with_capacity(num_lines);
    for _ in 0..num_lines {
        anyhow::ensure!(bytes.len() >= pos + 9, "EWKB truncated in MultiLineString member header");
        anyhow::ensure!(bytes[pos] == 1, "only little-endian EWKB is supported");
        let member_type = u32::from_le_bytes(bytes[pos + 1..pos + 5].try_into().unwrap());
        anyhow::ensure!(member_type == 2, "expected plain LineString member, got type {member_type:#x}");
        let num_points = u32::from_le_bytes(bytes[pos + 5..pos + 9].try_into().unwrap()) as usize;
        pos += 9;
        anyhow::ensure!(bytes.len() >= pos + num_points * 16, "EWKB truncated in MultiLineString member points");
        let mut coords = Vec::with_capacity(num_points);
        for i in 0..num_points {
            let off = pos + i * 16;
            let x = f64::from_le_bytes(bytes[off..off + 8].try_into().unwrap());
            let y = f64::from_le_bytes(bytes[off + 8..off + 16].try_into().unwrap());
            coords.push((x, y));
        }
        pos += num_points * 16;
        lines.push(coords);
    }
    Ok(lines)
}

/// Encode a projected (EPSG:3857) LineString as PostGIS EWKB with SRID.
pub fn to_ewkb(line: &LineString<f64>) -> Vec<u8> {
    use std::io::Write;

    let coords: Vec<_> = line.coords().collect();
    let num_points = coords.len() as u32;

    // WKB type for LineString with SRID flag: 0x20000002
    let wkb_type: u32 = 0x2000_0002;
    let srid: i32 = 3857;

    let mut buf: Vec<u8> = Vec::with_capacity(9 + 16 * coords.len());

    buf.write_all(&[1u8]).unwrap(); // little-endian byte order
    buf.write_all(&wkb_type.to_le_bytes()).unwrap();
    buf.write_all(&srid.to_le_bytes()).unwrap();
    buf.write_all(&num_points.to_le_bytes()).unwrap();
    for c in &coords {
        buf.write_all(&c.x.to_le_bytes()).unwrap();
        buf.write_all(&c.y.to_le_bytes()).unwrap();
    }
    buf
}

/// Encode a set of projected (EPSG:3857) LineStrings as one PostGIS EWKB MultiLineString with SRID
/// — the SRID flag/value lives only on the outer header, per-member sub-geometries are plain
/// (unflagged) `LineString` WKB, same nesting `polygon_to_ewkb` uses for its rings.
pub fn to_multi_ewkb(lines: &[LineString<f64>]) -> Vec<u8> {
    use std::io::Write;

    // WKB type for MultiLineString with SRID flag: 0x20000005
    let wkb_type: u32 = 0x2000_0005;
    let srid: i32 = 3857;
    let plain_linestring_type: u32 = 2;
    let num_lines = lines.len() as u32;

    let total_points: usize = lines.iter().map(|l| l.0.len()).sum();
    let mut buf: Vec<u8> = Vec::with_capacity(13 + 9 * lines.len() + 16 * total_points);
    buf.write_all(&[1u8]).unwrap();
    buf.write_all(&wkb_type.to_le_bytes()).unwrap();
    buf.write_all(&srid.to_le_bytes()).unwrap();
    buf.write_all(&num_lines.to_le_bytes()).unwrap();
    for line in lines {
        let coords: Vec<_> = line.coords().collect();
        buf.write_all(&[1u8]).unwrap();
        buf.write_all(&plain_linestring_type.to_le_bytes()).unwrap();
        buf.write_all(&(coords.len() as u32).to_le_bytes()).unwrap();
        for c in &coords {
            buf.write_all(&c.x.to_le_bytes()).unwrap();
            buf.write_all(&c.y.to_le_bytes()).unwrap();
        }
    }
    buf
}

#[cfg(test)]
mod multi_ewkb_tests {
    use super::*;

    #[test]
    fn round_trips_multiple_runs() {
        let lines = vec![
            project_line(&[(1.0, 2.0), (3.0, 4.0), (5.0, 6.0)]),
            project_line(&[(10.0, 20.0), (30.0, 40.0)]),
        ];
        let encoded = to_multi_ewkb(&lines);
        let decoded = multilinestring_from_ewkb(&encoded).unwrap();

        let expected: Vec<Vec<(f64, f64)>> =
            lines.iter().map(|l| l.coords().map(|c| (c.x, c.y)).collect()).collect();
        assert_eq!(decoded, expected);
    }

    #[test]
    fn round_trips_single_run() {
        let lines = vec![project_line(&[(0.0, 0.0), (1.0, 1.0)])];
        let encoded = to_multi_ewkb(&lines);
        let decoded = multilinestring_from_ewkb(&encoded).unwrap();
        assert_eq!(decoded.len(), 1);
        assert_eq!(decoded[0].len(), 2);
    }

    #[test]
    fn rejects_wrong_type() {
        let line = project_line(&[(0.0, 0.0), (1.0, 1.0)]);
        let single_line_ewkb = to_ewkb(&line);
        assert!(multilinestring_from_ewkb(&single_line_ewkb).is_err());
    }
}
