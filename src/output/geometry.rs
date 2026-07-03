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
