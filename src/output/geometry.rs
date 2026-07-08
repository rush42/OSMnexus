use geo::{Length, LineString, Haversine};

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
