//! The pipeline's geometry intermediate representation: coordinates exactly as the PBF stores them.
//!
//! OSM coordinates are fixed-point integers in 1e-7 degrees ("decimicrodegrees"). Every shape the
//! pipeline produces from them — a node's point, a way's line or ring, a relation's assembled runs
//! or rings, a graph edge — is a selection or concatenation of those integer pairs, never new
//! arithmetic, so [`Geom`] carries them unchanged as [`Coord`]s: exact, 8 bytes per point, and
//! cheap to stage.
//!
//! Conversion happens once, at the sink, into whatever that sink needs:
//!
//! - Postgres/CSV: Web Mercator EWKB (`Geom::to_ewkb_3857`) — the `geometry(…, 3857)` columns.
//! - Parquet: plain lon/lat WKB, at the run's coordinate precision (`output::coord_fmt`).
//! - GeoJSON: decimal text written straight from the integers, no float in between
//!   (`output::coord_fmt`).
//!
//! This replaced projecting to 3857 up front and staging EWKB, which made every file output decode
//! the EWKB, invert the projection point by point (`exp`/`atan`), and round the inversion's noise
//! back off — undoing work no file output wanted done.
//!
//! The one exception is *derived* geometry: a centroid is computed, not selected, so it is a float
//! from birth and is carried as one ([`Geom::Centroid`], lon/lat degrees). Quantizing it to 1e-7
//! would buy nothing — it's a single point per element — and would round away precision the
//! computation produced.

use geo::Centroid;

use crate::geom::primitives::{mercator_to_wgs84, wgs84_to_3857};

/// `(lon, lat)` in decimicrodegrees (1e-7 degrees), as `osmpbf`'s `decimicro_lon`/`decimicro_lat`
/// hand them out.
pub type Coord = [i32; 2];

/// One decimicrodegree, in degrees.
pub const DECIMICRO: f64 = 1e-7;

/// `c` in degrees. Multiplication (not division by 1e7) because it is what the pipeline has always
/// used to turn the stored integers into degrees — keeping it means projected output (`to_ewkb_3857`)
/// is bit-for-bit what it was when coordinates were held as degrees.
#[inline]
pub fn to_deg(c: Coord) -> (f64, f64) {
    (c[0] as f64 * DECIMICRO, c[1] as f64 * DECIMICRO)
}

/// A geometry in the pipeline's IR — see the module doc.
#[derive(Clone, Debug, PartialEq)]
pub enum Geom {
    /// A node's own coordinate.
    Point(Coord),
    /// A derived point (a way's or relation's centroid), lon/lat degrees.
    Centroid([f64; 2]),
    Line(Vec<Coord>),
    /// A relation's member ways chained into disconnected runs (see
    /// `geom::builders::build_relation_line_row`).
    MultiLine(Vec<Vec<Coord>>),
    /// Closed rings, exterior first, then holes.
    Polygon(Vec<Vec<Coord>>),
}

impl Geom {
    /// The GeoJSON/OGC type name. A centroid is a `Point` to every consumer; only the IR
    /// distinguishes it, by coordinate type.
    pub fn geom_type(&self) -> &'static str {
        match self {
            Geom::Point(_) | Geom::Centroid(_) => "Point",
            Geom::Line(_) => "LineString",
            Geom::MultiLine(_) => "MultiLineString",
            Geom::Polygon(_) => "Polygon",
        }
    }

    /// PostGIS EWKB in EPSG:3857 (little-endian, SRID-flagged) — the Postgres/CSV encoding.
    pub fn to_ewkb_3857(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(self.wkb_len(true));
        write_wkb(&mut buf, self, Some(3857), project, project_f);
        buf
    }

    /// Exact size of this geometry's (E)WKB encoding, for preallocating.
    pub fn wkb_len(&self, srid: bool) -> usize {
        let header = 5 + if srid { 4 } else { 0 };
        match self {
            Geom::Point(_) | Geom::Centroid(_) => header + 16,
            Geom::Line(l) => header + 4 + 16 * l.len(),
            Geom::Polygon(rings) => header + 4 + rings.iter().map(|r| 4 + 16 * r.len()).sum::<usize>(),
            Geom::MultiLine(lines) => header + 4 + lines.iter().map(|l| 9 + 16 * l.len()).sum::<usize>(),
        }
    }
}

/// EWKB 3857 for a bare line, without building a `Geom` (and copying the points) first — for
/// `EdgeRow`, which holds its segment as a plain `Vec<Coord>`.
pub fn line_ewkb_3857(line: &[Coord]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(13 + 16 * line.len());
    buf.push(1);
    buf.extend_from_slice(&0x2000_0002u32.to_le_bytes());
    buf.extend_from_slice(&3857u32.to_le_bytes());
    buf.extend_from_slice(&(line.len() as u32).to_le_bytes());
    for &c in line {
        let [x, y] = project(c);
        buf.extend_from_slice(&x.to_le_bytes());
        buf.extend_from_slice(&y.to_le_bytes());
    }
    buf
}

/// `c` in EPSG:3857.
#[inline]
fn project(c: Coord) -> [f64; 2] {
    let (lon, lat) = to_deg(c);
    let (x, y) = wgs84_to_3857(lon, lat);
    [x, y]
}

#[inline]
fn project_f(p: [f64; 2]) -> [f64; 2] {
    let (x, y) = wgs84_to_3857(p[0], p[1]);
    [x, y]
}

/// Little-endian (E)WKB for `geom`, coordinates mapped through `map` (integer points) and `map_f`
/// (a centroid's float point). `srid: Some(_)` writes PostGIS's SRID-flagged EWKB header; `None`
/// writes plain OGC WKB. Sub-geometries (a `MultiLineString`'s members) are always plain, with the
/// SRID only on the outer header — the layout PostGIS itself writes.
pub fn write_wkb(
    buf: &mut Vec<u8>,
    geom: &Geom,
    srid: Option<u32>,
    map: impl Fn(Coord) -> [f64; 2],
    map_f: impl Fn([f64; 2]) -> [f64; 2],
) {
    const SRID_FLAG: u32 = 0x2000_0000;
    let type_code = match geom {
        Geom::Point(_) | Geom::Centroid(_) => 1u32,
        Geom::Line(_) => 2,
        Geom::Polygon(_) => 3,
        Geom::MultiLine(_) => 5,
    };
    buf.push(1);
    match srid {
        Some(srid) => {
            buf.extend_from_slice(&(type_code | SRID_FLAG).to_le_bytes());
            buf.extend_from_slice(&srid.to_le_bytes());
        }
        None => buf.extend_from_slice(&type_code.to_le_bytes()),
    }
    let put = |buf: &mut Vec<u8>, p: [f64; 2]| {
        buf.extend_from_slice(&p[0].to_le_bytes());
        buf.extend_from_slice(&p[1].to_le_bytes());
    };
    let put_run = |buf: &mut Vec<u8>, run: &[Coord]| {
        buf.extend_from_slice(&(run.len() as u32).to_le_bytes());
        for &c in run {
            put(buf, map(c));
        }
    };
    match geom {
        Geom::Point(c) => put(buf, map(*c)),
        Geom::Centroid(p) => put(buf, map_f(*p)),
        Geom::Line(line) => put_run(buf, line),
        Geom::Polygon(rings) => {
            buf.extend_from_slice(&(rings.len() as u32).to_le_bytes());
            for ring in rings {
                put_run(buf, ring);
            }
        }
        Geom::MultiLine(lines) => {
            buf.extend_from_slice(&(lines.len() as u32).to_le_bytes());
            for line in lines {
                buf.push(1);
                buf.extend_from_slice(&2u32.to_le_bytes());
                put_run(buf, line);
            }
        }
    }
}

fn projected_line(coords: &[Coord]) -> geo::LineString<f64> {
    coords
        .iter()
        .map(|&c| {
            let [x, y] = project(c);
            geo::coord! { x: x, y: y }
        })
        .collect()
}

fn unproject_point(p: geo::Point<f64>) -> [f64; 2] {
    let (lon, lat) = mercator_to_wgs84(p.x(), p.y());
    [lon, lat]
}

/// Centroid of a coordinate sequence read as a line (`geo::Centroid` on a `LineString`:
/// length-weighted segment midpoints), lon/lat degrees. Computed in Web Mercator, as it always
/// was: a planar centroid in raw lon/lat would underweight east-west segments by `cos(lat)`, while
/// Mercator is locally true to scale — and is what a web map draws, so the result is the visual
/// centre there. `None` for an empty sequence.
pub fn centroid(coords: &[Coord]) -> Option<[f64; 2]> {
    projected_line(coords).centroid().map(unproject_point)
}

/// Centroid of several disjoint lines taken together (`geo::Centroid` on a `MultiLineString`:
/// every run's segments length-weighted, no segment joining one run to the next), lon/lat degrees,
/// computed in Web Mercator like [`centroid`]. For a relation, whose member ways are not one line:
/// concatenating them would add a phantom segment from each member's end to the next member's
/// start, and with members listed out of geographic order those chords can outweigh the real
/// geometry. `None` if there are no points at all.
pub fn centroid_of_runs(runs: &[Vec<Coord>]) -> Option<[f64; 2]> {
    let multi = geo::MultiLineString::new(runs.iter().map(|r| projected_line(r)).collect());
    multi.centroid().map(unproject_point)
}

/// Close a ring in place (repeat the first point at the end if it isn't already there).
pub fn close_ring(ring: &mut Vec<Coord>) {
    if let (Some(&first), Some(&last)) = (ring.first(), ring.last()) {
        if first != last {
            ring.push(first);
        }
    }
}

// --- staging encoding ----------------------------------------------------------------------------
//
// One tag byte, then the payload. Integer runs are a `u32` point count followed by the raw
// little-endian `i32` pairs — the in-memory layout, so decoding is a straight copy.

const TAG_POINT: u8 = 0;
const TAG_CENTROID: u8 = 1;
const TAG_LINE: u8 = 2;
const TAG_MULTILINE: u8 = 3;
const TAG_POLYGON: u8 = 4;

fn put_run(buf: &mut Vec<u8>, run: &[Coord]) {
    buf.extend_from_slice(&(run.len() as u32).to_le_bytes());
    buf.reserve(run.len() * 8);
    for c in run {
        buf.extend_from_slice(&c[0].to_le_bytes());
        buf.extend_from_slice(&c[1].to_le_bytes());
    }
}

pub fn stage_encode_run(buf: &mut Vec<u8>, run: &[Coord]) {
    put_run(buf, run);
}

pub fn stage_encode(buf: &mut Vec<u8>, geom: &Geom) {
    match geom {
        Geom::Point(c) => {
            buf.push(TAG_POINT);
            buf.extend_from_slice(&c[0].to_le_bytes());
            buf.extend_from_slice(&c[1].to_le_bytes());
        }
        Geom::Centroid(p) => {
            buf.push(TAG_CENTROID);
            buf.extend_from_slice(&p[0].to_le_bytes());
            buf.extend_from_slice(&p[1].to_le_bytes());
        }
        Geom::Line(line) => {
            buf.push(TAG_LINE);
            put_run(buf, line);
        }
        Geom::MultiLine(runs) | Geom::Polygon(runs) => {
            buf.push(if matches!(geom, Geom::MultiLine(_)) { TAG_MULTILINE } else { TAG_POLYGON });
            buf.extend_from_slice(&(runs.len() as u32).to_le_bytes());
            for run in runs {
                put_run(buf, run);
            }
        }
    }
}

/// Reads what [`stage_encode`] wrote, starting at `buf[*pos]`, advancing `pos` past it.
pub struct GeomDecoder<'a> {
    pub buf: &'a [u8],
    pub pos: usize,
}

impl<'a> GeomDecoder<'a> {
    fn take(&mut self, n: usize) -> anyhow::Result<&'a [u8]> {
        let end = self.pos.checked_add(n).filter(|&e| e <= self.buf.len());
        let Some(end) = end else { anyhow::bail!("staged geometry truncated") };
        let s = &self.buf[self.pos..end];
        self.pos = end;
        Ok(s)
    }

    fn u32(&mut self) -> anyhow::Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into()?))
    }

    fn coord(bytes: &[u8]) -> Coord {
        [i32::from_le_bytes(bytes[0..4].try_into().unwrap()), i32::from_le_bytes(bytes[4..8].try_into().unwrap())]
    }

    pub fn run(&mut self) -> anyhow::Result<Vec<Coord>> {
        let n = self.u32()? as usize;
        let bytes = self.take(n * 8)?;
        Ok(bytes.chunks_exact(8).map(Self::coord).collect())
    }

    fn runs(&mut self) -> anyhow::Result<Vec<Vec<Coord>>> {
        let n = self.u32()? as usize;
        (0..n).map(|_| self.run()).collect()
    }

    pub fn geom(&mut self) -> anyhow::Result<Geom> {
        let tag = self.take(1)?[0];
        Ok(match tag {
            TAG_POINT => Geom::Point(Self::coord(self.take(8)?)),
            TAG_CENTROID => {
                let b = self.take(16)?;
                Geom::Centroid([f64::from_le_bytes(b[0..8].try_into()?), f64::from_le_bytes(b[8..16].try_into()?)])
            }
            TAG_LINE => Geom::Line(self.run()?),
            TAG_MULTILINE => Geom::MultiLine(self.runs()?),
            TAG_POLYGON => Geom::Polygon(self.runs()?),
            other => anyhow::bail!("unknown staged geometry tag {other}"),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two member ways far apart, in either member order: the centroid is the same, and it is the
    /// length-weighted mean of the two members alone — the chord a concatenation would draw between
    /// them carries no weight.
    #[test]
    fn centroid_of_runs_ignores_gaps_between_runs() {
        let d = 10_000; // 0.001° — small enough that Mercator is near-linear here
        let a = vec![[0, 0], [d, 0]];
        let b = vec![[100 * d, 100 * d], [100 * d, 101 * d]];
        let ab = centroid_of_runs(&[a.clone(), b.clone()]).unwrap();
        let ba = centroid_of_runs(&[b.clone(), a.clone()]).unwrap();
        assert!((ab[0] - ba[0]).abs() < 1e-12 && (ab[1] - ba[1]).abs() < 1e-12);

        // Equal-length members: the midpoint of the two members' midpoints (0.0005, 0) and
        // (0.1, 0.1005) — not pulled towards the 0.14°-long chord between them.
        assert!((ab[0] - 0.05025).abs() < 1e-6, "{ab:?}");
        assert!((ab[1] - 0.05025).abs() < 1e-6, "{ab:?}");

        let concatenated: Vec<Coord> = a.iter().chain(&b).copied().collect();
        let pooled = centroid(&concatenated).unwrap();
        assert!((pooled[0] - ab[0]).abs() > 1e-4, "the concatenation's chord should have shifted it");
    }

    fn round_trip(g: &Geom) -> Geom {
        let mut buf = Vec::new();
        stage_encode(&mut buf, g);
        let mut d = GeomDecoder { buf: &buf, pos: 0 };
        let out = d.geom().unwrap();
        assert_eq!(d.pos, buf.len());
        out
    }

    #[test]
    fn every_variant_stage_round_trips() {
        let a = [134_049_000, 525_200_000];
        let b = [-1_800_000_000, 900_000_000];
        for g in [
            Geom::Point(a),
            Geom::Centroid([13.4049123456789, 52.52000000000001]),
            Geom::Line(vec![a, b]),
            Geom::MultiLine(vec![vec![a, b], vec![b, a, b]]),
            Geom::Polygon(vec![vec![a, b, [0, 0], a], vec![]]),
        ] {
            assert_eq!(round_trip(&g), g);
        }
    }

    /// The bytes Postgres receives must not change: they are what `geom::primitives`' EWKB writers
    /// produced from the same points held as degrees.
    #[test]
    fn ewkb_matches_the_previous_writers() {
        use crate::geom::primitives::legacy::{point_to_ewkb, polygon_to_ewkb, project_line, project_polygon, to_ewkb, to_multi_ewkb};
        let pts: Vec<Coord> = vec![[134_049_000, 525_200_000], [134_050_001, 525_201_234], [134_060_000, 525_190_000]];
        let deg: Vec<(f64, f64)> = pts.iter().map(|&c| to_deg(c)).collect();

        assert_eq!(Geom::Line(pts.clone()).to_ewkb_3857(), to_ewkb(&project_line(&deg)));

        let (x, y) = wgs84_to_3857(deg[0].0, deg[0].1);
        assert_eq!(Geom::Point(pts[0]).to_ewkb_3857(), point_to_ewkb(x, y));

        let mut ring = pts.clone();
        close_ring(&mut ring);
        assert_eq!(Geom::Polygon(vec![ring]).to_ewkb_3857(), polygon_to_ewkb(&project_polygon(&deg, &[])));

        let multi = Geom::MultiLine(vec![pts.clone(), pts[..2].to_vec()]);
        assert_eq!(multi.to_ewkb_3857(), to_multi_ewkb(&[project_line(&deg), project_line(&deg[..2])]));

        assert_eq!(line_ewkb_3857(&pts), Geom::Line(pts.clone()).to_ewkb_3857());

        for g in [Geom::Line(pts.clone()), Geom::Point(pts[0]), multi] {
            assert_eq!(g.to_ewkb_3857().len(), g.wkb_len(true));
        }
    }
}
