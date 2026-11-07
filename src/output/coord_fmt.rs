//! The file outputs' coordinate encodings — where the geometry IR's integer coordinates (see
//! `geom::coord`) first become something else, at the run's `--coordinate-precision`:
//!
//! - GeoJSON text, written straight from the integers: `531016340` is `53.101634` by inserting a
//!   decimal point, so no float is formatted (and no `ryu` shortest-round-trip search runs) for any
//!   raw coordinate. The text is exactly what serializing the correctly rounded `f64` would give —
//!   see `int_text_matches_float_serialization` — so this is a faster route to the same bytes, not
//!   a different format.
//! - Plain lon/lat WKB for GeoParquet's `geometry` column.
//!
//! Precision on an integer is integer rounding: below 7 decimals the value is rounded half away from
//! zero (`f64::round`'s rule) to `10^-p`; at 7 or above it is the stored value unchanged, since 7 is
//! the source's own resolution. A centroid ([`Geom::Centroid`]) is a float and is rounded with
//! `CoordPrecision::round` as before.

use crate::config::CoordPrecision;
use crate::geom::coord::{write_wkb, Coord, Geom};

/// The source resolution: decimicrodegrees, 7 decimal places.
const SOURCE_DECIMALS: u32 = 7;

#[derive(Clone, Copy, Debug)]
pub struct CoordFmt {
    precision: CoordPrecision,
    /// Decimals actually kept for an integer coordinate: `min(precision, 7)`.
    decimals: u32,
    /// `10^(7 - decimals)` — the integer rounding step.
    step: i64,
    /// `10^decimals`, as both integer and float.
    unit: i64,
    unit_f: f64,
}

impl CoordFmt {
    pub fn new(precision: CoordPrecision) -> Self {
        let decimals = precision.0.min(SOURCE_DECIMALS);
        let unit = 10i64.pow(decimals);
        CoordFmt { precision, decimals, step: 10i64.pow(SOURCE_DECIMALS - decimals), unit, unit_f: unit as f64 }
    }

    /// `v` in units of `10^-decimals`, rounded half away from zero.
    #[inline]
    fn quantize(&self, v: i32) -> i64 {
        let v = v as i64;
        if self.step == 1 {
            return v;
        }
        let half = self.step / 2;
        if v >= 0 { (v + half) / self.step } else { -((-v + half) / self.step) }
    }

    /// One coordinate value in degrees — the correctly rounded `f64` of the quantized decimal.
    #[cfg_attr(not(feature = "parquet"), allow(dead_code))]
    #[inline]
    fn value(&self, v: i32) -> f64 {
        self.quantize(v) as f64 / self.unit_f
    }

    /// `c` as lon/lat degrees at this precision.
    #[cfg_attr(not(feature = "parquet"), allow(dead_code))]
    #[inline]
    pub fn lonlat(&self, c: Coord) -> [f64; 2] {
        [self.value(c[0]), self.value(c[1])]
    }

    /// A centroid's float point at this precision.
    #[inline]
    pub fn lonlat_f(&self, p: [f64; 2]) -> [f64; 2] {
        [self.precision.round(p[0]), self.precision.round(p[1])]
    }

    /// Plain (SRID-less) little-endian lon/lat WKB — GeoParquet's `geometry` column encoding; the
    /// CRS is declared once in file metadata instead (`output::parquet::geoparquet_crs84`).
    #[cfg_attr(not(feature = "parquet"), allow(dead_code))]
    pub fn write_wkb(&self, buf: &mut Vec<u8>, geom: &Geom) {
        buf.reserve(geom.wkb_len(false));
        write_wkb(buf, geom, None, |c| self.lonlat(c), |p| self.lonlat_f(p));
    }

    /// Plain lon/lat WKB `LineString` for a bare run — a graph-edge segment, which the cursors hold
    /// as `&[Coord]` rather than a `Geom`.
    #[cfg_attr(not(feature = "parquet"), allow(dead_code))]
    pub fn write_wkb_line(&self, buf: &mut Vec<u8>, run: &[Coord]) {
        buf.reserve(9 + 16 * run.len());
        buf.push(1);
        buf.extend_from_slice(&2u32.to_le_bytes());
        buf.extend_from_slice(&(run.len() as u32).to_le_bytes());
        for &c in run {
            let [lon, lat] = self.lonlat(c);
            buf.extend_from_slice(&lon.to_le_bytes());
            buf.extend_from_slice(&lat.to_le_bytes());
        }
    }

    /// One JSON number for one integer coordinate value.
    fn write_value(&self, buf: &mut Vec<u8>, v: i32) {
        let q = self.quantize(v);
        let abs = q.unsigned_abs();
        // Below 1° the float formatter may pick exponent notation (`1e-7`); rare enough to defer to
        // it rather than replicate its rules. At or above 1° it always writes plain decimal.
        if abs < self.unit as u64 {
            write_f64(buf, q as f64 / self.unit_f);
            return;
        }
        if q < 0 {
            buf.push(b'-');
        }
        let unit = self.unit as u64;
        let (int, mut frac) = (abs / unit, abs % unit);
        let mut digits = itoa_buf(int);
        buf.extend_from_slice(digits.as_bytes());
        buf.push(b'.');
        if frac == 0 {
            buf.push(b'0');
            return;
        }
        // `frac` has `decimals` digits with leading zeros; drop the trailing ones.
        let mut width = self.decimals as usize;
        while frac % 10 == 0 {
            frac /= 10;
            width -= 1;
        }
        digits = itoa_buf(frac);
        for _ in digits.len()..width {
            buf.push(b'0');
        }
        buf.extend_from_slice(digits.as_bytes());
    }

    /// `[lon,lat]`.
    #[inline]
    pub fn write_json_coord(&self, buf: &mut Vec<u8>, c: Coord) {
        buf.push(b'[');
        self.write_value(buf, c[0]);
        buf.push(b',');
        self.write_value(buf, c[1]);
        buf.push(b']');
    }

    fn write_json_point_f(&self, buf: &mut Vec<u8>, p: [f64; 2]) {
        let [lon, lat] = self.lonlat_f(p);
        buf.push(b'[');
        write_f64(buf, lon);
        buf.push(b',');
        write_f64(buf, lat);
        buf.push(b']');
    }

    /// `[[lon,lat],…]`.
    pub fn write_json_run(&self, buf: &mut Vec<u8>, run: &[Coord]) {
        buf.push(b'[');
        for (i, &c) in run.iter().enumerate() {
            if i > 0 {
                buf.push(b',');
            }
            self.write_json_coord(buf, c);
        }
        buf.push(b']');
    }

    fn write_json_runs(&self, buf: &mut Vec<u8>, runs: &[Vec<Coord>]) {
        buf.push(b'[');
        for (i, run) in runs.iter().enumerate() {
            if i > 0 {
                buf.push(b',');
            }
            self.write_json_run(buf, run);
        }
        buf.push(b']');
    }

    /// A GeoJSON geometry object, `{"coordinates":…,"type":…}` — keys sorted, compact, as
    /// `serde_json` serialized it before.
    pub fn write_json_geometry(&self, buf: &mut Vec<u8>, geom: &Geom) {
        buf.extend_from_slice(br#"{"coordinates":"#);
        match geom {
            Geom::Point(c) => self.write_json_coord(buf, *c),
            Geom::Centroid(p) => self.write_json_point_f(buf, *p),
            Geom::Line(line) => self.write_json_run(buf, line),
            Geom::MultiLine(runs) | Geom::Polygon(runs) => self.write_json_runs(buf, runs),
        }
        buf.extend_from_slice(br#","type":""#);
        buf.extend_from_slice(geom.geom_type().as_bytes());
        buf.extend_from_slice(br#""}"#);
    }
}

/// `v` exactly as `serde_json` serializes an `f64`.
fn write_f64(buf: &mut Vec<u8>, v: f64) {
    serde_json::to_writer(&mut *buf, &v).expect("writing to a Vec cannot fail");
}

/// Decimal digits of `v`.
fn itoa_buf(v: u64) -> ItoaBuf {
    let mut out = ItoaBuf { bytes: [0; 20], start: 20 };
    let mut v = v;
    loop {
        out.start -= 1;
        out.bytes[out.start] = b'0' + (v % 10) as u8;
        v /= 10;
        if v == 0 {
            break;
        }
    }
    out
}

struct ItoaBuf {
    bytes: [u8; 20],
    start: usize,
}

impl ItoaBuf {
    fn as_bytes(&self) -> &[u8] {
        &self.bytes[self.start..]
    }

    fn len(&self) -> usize {
        20 - self.start
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(fmt: &CoordFmt, v: i32) -> String {
        let mut buf = Vec::new();
        fmt.write_value(&mut buf, v);
        String::from_utf8(buf).unwrap()
    }

    /// The fast integer path must produce exactly the text the float path would: `serde_json` on
    /// the correctly rounded `f64` of the same decimal. Checked across every precision, both signs,
    /// round values, sub-degree values and the coordinate range's extremes, plus a pseudo-random
    /// sweep.
    #[test]
    fn int_text_matches_float_serialization() {
        let mut values: Vec<i32> = vec![
            0, 1, -1, 9_999_999, 10_000_000, -10_000_000, 10_000_001, 134_049_000, 525_200_000, 531_016_340,
            -531_016_340, 1_800_000_000, -1_800_000_000, 900_000_000, -900_000_000, 123_456_789, 10_500_000,
            10_050_000, 10_005_000, 15_000_000, -15_000_000, 25_000_000,
        ];
        let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
        for _ in 0..200_000 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            values.push(((x % 3_600_000_001) as i64 - 1_800_000_000) as i32);
        }
        for p in 0..=21 {
            let fmt = CoordFmt::new(CoordPrecision(p));
            for &v in &values {
                let mut expected = Vec::new();
                write_f64(&mut expected, fmt.value(v));
                assert_eq!(text(&fmt, v), String::from_utf8(expected).unwrap(), "v={v} p={p}");
            }
        }
    }

    #[test]
    fn precision_rounds_half_away_from_zero() {
        let fmt = CoordFmt::new(CoordPrecision(3));
        assert_eq!(text(&fmt, 134_045_000), "13.405");
        assert_eq!(text(&fmt, -134_045_000), "-13.405");
        assert_eq!(text(&fmt, 134_044_999), "13.404");
        assert_eq!(text(&fmt, 129_995_000), "13.0");
    }

    /// At or above the source resolution an integer coordinate is its stored value — the noise the
    /// old projection round trip left (`53.101634000000004`) never arises.
    #[test]
    fn full_precision_is_the_stored_value() {
        for p in [7, 17, 21] {
            let fmt = CoordFmt::new(CoordPrecision(p));
            assert_eq!(text(&fmt, 531_016_340), "53.101634");
            assert_eq!(fmt.lonlat([88_805_731, 531_016_340]), [8.8805731, 53.101634]);
        }
    }

    #[test]
    fn geometry_json_shapes() {
        let fmt = CoordFmt::new(CoordPrecision(21));
        let mut buf = Vec::new();
        fmt.write_json_geometry(&mut buf, &Geom::Polygon(vec![vec![[10_000_000, 20_000_000], [30_000_000, 40_000_000]]]));
        assert_eq!(String::from_utf8(buf).unwrap(), r#"{"coordinates":[[[1.0,2.0],[3.0,4.0]]],"type":"Polygon"}"#);

        let mut buf = Vec::new();
        fmt.write_json_geometry(&mut buf, &Geom::Centroid([13.5, 52.25]));
        assert_eq!(String::from_utf8(buf).unwrap(), r#"{"coordinates":[13.5,52.25],"type":"Point"}"#);
    }

    #[test]
    fn wkb_is_plain_lonlat() {
        let fmt = CoordFmt::new(CoordPrecision(21));
        let mut wkb = Vec::new();
        fmt.write_wkb(&mut wkb, &Geom::Point([134_000_000, 525_000_000]));
        assert_eq!(wkb[0], 1);
        assert_eq!(u32::from_le_bytes(wkb[1..5].try_into().unwrap()), 1); // Point, no SRID flag
        assert_eq!(f64::from_le_bytes(wkb[5..13].try_into().unwrap()), 13.4);
        assert_eq!(f64::from_le_bytes(wkb[13..21].try_into().unwrap()), 52.5);

        let line = vec![[0, 0], [10_000_000, 10_000_000]];
        let (mut a, mut b) = (Vec::new(), Vec::new());
        fmt.write_wkb_line(&mut a, &line);
        fmt.write_wkb(&mut b, &Geom::Line(line));
        assert_eq!(a, b);

        let mut wkb = Vec::new();
        fmt.write_wkb(&mut wkb, &Geom::MultiLine(vec![vec![[0, 0], [1, 1]], vec![[2, 2], [3, 3]]]));
        assert_eq!(u32::from_le_bytes(wkb[1..5].try_into().unwrap()), 5);
        assert_eq!(u32::from_le_bytes(wkb[5..9].try_into().unwrap()), 2);
    }
}
