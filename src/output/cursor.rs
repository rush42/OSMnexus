//! Shared forward-cursor join machinery for output backends that re-read the staged tag/geometry/
//! edges tables after the main run finishes (`output::geojson`'s `.geojson`/`.geojsonseq`,
//! `output::parquet`'s `.parquet`) — see `output::stage` for the file format these read, and
//! `output::geojson`'s own (fuller) doc for why walking these files forward, in lockstep, needs no
//! hashmap for node/way/relation geometry or a way's own graph-fallback edges (only a relation's
//! graph-fallback edges, which need random access by arbitrary member way id, still do).

use std::collections::HashMap;
use std::io::Write;
use std::path::Path;

use crate::geom::primitives::{linestring_from_ewkb, mercator_to_wgs84, multilinestring_from_ewkb, point_from_ewkb};
use crate::geom::rows::{EdgeRow, GeomRow};
use crate::output::rows::MemberRow;
use crate::output::stage::StageReader;

pub struct EdgeGeom {
    pub seg_idx: usize,
    // Decoded once in `read_edges`, not per feature — a way shared by many relations (e.g. a
    // trunk road several bus routes run down) would otherwise get its WKB re-parsed and
    // re-projected once per relation that references it.
    pub coordinates: Vec<[f64; 2]>,
}

fn lonlat_coordinates(geom: &[u8]) -> anyhow::Result<Vec<[f64; 2]>> {
    Ok(linestring_from_ewkb(geom)?
        .into_iter()
        .map(|(x, y)| {
            let (lon, lat) = mercator_to_wgs84(x, y);
            [lon, lat]
        })
        .collect())
}

fn lonlat_multi_coordinates(geom: &[u8]) -> anyhow::Result<Vec<Vec<[f64; 2]>>> {
    Ok(multilinestring_from_ewkb(geom)?
        .into_iter()
        .map(|run| {
            run.into_iter()
                .map(|(x, y)| {
                    let (lon, lat) = mercator_to_wgs84(x, y);
                    [lon, lat]
                })
                .collect()
        })
        .collect())
}

fn lonlat_point(geom: &[u8]) -> anyhow::Result<[f64; 2]> {
    let (x, y) = point_from_ewkb(geom)?;
    let (lon, lat) = mercator_to_wgs84(x, y);
    Ok([lon, lat])
}

/// Reads `relation_members.bin` (`relation_osm_id,way_osm_id`), keyed by `relation_osm_id`. Empty
/// map if the file doesn't exist — no topic declared relation categories at all.
pub fn read_relation_members(path: &Path) -> anyhow::Result<HashMap<i64, Vec<i64>>> {
    if !path.exists() {
        return Ok(HashMap::new());
    }
    let mut reader = StageReader::<MemberRow>::open(path)?;
    let mut by_relation_id: HashMap<i64, Vec<i64>> = HashMap::new();
    while let Some(row) = reader.next_row()? {
        by_relation_id.entry(row.relation_osm_id).or_default().push(row.way_osm_id);
    }
    Ok(by_relation_id)
}

/// Reads `edges.bin` into one flat sequence, in file order — which is `kept_way_order` order (see
/// `output::geojson`'s own doc), i.e. every way's own segments are contiguous and `seg_idx`-ascending
/// already, with no re-sort needed. Empty if the file doesn't exist — no topic declared `"graph": {
/// "way": true }`.
pub fn read_edges(path: &Path) -> anyhow::Result<Vec<(i64, EdgeGeom)>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let mut reader = StageReader::<EdgeRow>::open(path)?;
    let mut out = Vec::new();
    while let Some(row) = reader.next_row()? {
        if row.geom_ewkb.is_empty() {
            continue; // a way whose shape degenerated to nothing (see `WayGeometry`'s own doc)
        }
        let coordinates = lonlat_coordinates(&row.geom_ewkb)?;
        out.push((row.osm_id, EdgeGeom { seg_idx: row.seg_idx, coordinates }));
    }
    Ok(out)
}

/// Groups `edges`' flat sequence by way id — for a relation's graph-fallback geometry, which needs
/// random access by arbitrary (non-adjacent) member way id and so can't use `EdgeCursor`'s forward
/// walk.
pub fn group_edges_by_way(edges: &[(i64, EdgeGeom)]) -> HashMap<i64, Vec<&EdgeGeom>> {
    let mut map: HashMap<i64, Vec<&EdgeGeom>> = HashMap::new();
    for (id, g) in edges {
        map.entry(*id).or_default().push(g);
    }
    map
}

/// Forward cursor over `edges`' flat sequence, consumed in step with `{table}.bin`'s own way rows —
/// the graph-fallback equivalent of `OrderedGeomCursor` for the *own-way* case (a way asking for its
/// own segments; see `group_edges_by_way`'s own doc for why the relation-member case still needs a
/// hashmap instead). `get_all`, not `get`: unlike node/way/relation geometry (one row, or a repeat of
/// the same one), a way can legitimately own several contiguous segments.
pub struct EdgeCursor<'a> {
    edges: &'a [(i64, EdgeGeom)],
    pos: usize,
    /// Same repeat-reuse reasoning as `OrderedGeomCursor::last` — a side-split topic asks for the
    /// same way's segments once per side.
    last: Option<(i64, Vec<&'a EdgeGeom>)>,
}

impl<'a> EdgeCursor<'a> {
    pub fn new(edges: &'a [(i64, EdgeGeom)]) -> Self {
        EdgeCursor { edges, pos: 0, last: None }
    }

    /// This way's segments, in order — empty if `edges` has none for it. Must be called with
    /// `osm_id` in the same relative order `{table}.bin`'s own-kind rows come in.
    pub fn get_all(&mut self, osm_id: i64) -> Vec<&'a EdgeGeom> {
        if let Some((id, segs)) = &self.last {
            if *id == osm_id {
                return segs.clone();
            }
        }
        if self.pos >= self.edges.len() || self.edges[self.pos].0 != osm_id {
            return Vec::new();
        }
        let start = self.pos;
        while self.pos < self.edges.len() && self.edges[self.pos].0 == osm_id {
            self.pos += 1;
        }
        let segs: Vec<&'a EdgeGeom> = self.edges[start..self.pos].iter().map(|(_, g)| g).collect();
        self.last = Some((osm_id, segs.clone()));
        segs
    }
}

/// One decoded row of a `{table}_node_geom`/`{table}_way_geom`/`{table}_relation_geom` file —
/// `geom_type` says which shape `coordinates` decodes to (`Point` → `[f64; 2]`, `LineString`/
/// `Polygon` → `Vec<[f64; 2]>`, `MultiLineString` → `Vec<Vec<[f64; 2]>>`, hence the enum rather than
/// one fixed shape). `to_geojson`/`to_wkb` are the two encodings its two consumers need — GeoJSON's
/// native JSON geometry object, and plain (SRID-less; the CRS is declared once in Parquet file
/// metadata instead — see `output::parquet::geoparquet_crs84`) little-endian WKB for GeoParquet's
/// `geometry` column.
pub enum GeomValue {
    Point([f64; 2]),
    Line(Vec<[f64; 2]>),
    MultiLine(Vec<Vec<[f64; 2]>>),
    Polygon(Vec<[f64; 2]>),
}

impl GeomValue {
    fn decode(geom_type: &str, geom_ewkb: &[u8]) -> anyhow::Result<Self> {
        Ok(match geom_type {
            "Point" => GeomValue::Point(lonlat_point(geom_ewkb)?),
            "LineString" => GeomValue::Line(lonlat_coordinates(geom_ewkb)?),
            "MultiLineString" => GeomValue::MultiLine(lonlat_multi_coordinates(geom_ewkb)?),
            "Polygon" => GeomValue::Polygon(lonlat_coordinates(geom_ewkb)?),
            other => anyhow::bail!("unknown geom_type {other:?} in geometry table"),
        })
    }

    pub fn geom_type(&self) -> &'static str {
        match self {
            GeomValue::Point(_) => "Point",
            GeomValue::Line(_) => "LineString",
            GeomValue::MultiLine(_) => "MultiLineString",
            GeomValue::Polygon(_) => "Polygon",
        }
    }

    pub fn to_geojson(&self) -> serde_json::Value {
        use serde_json::json;
        match self {
            GeomValue::Point(p) => json!({ "type": "Point", "coordinates": p }),
            GeomValue::Line(l) => json!({ "type": "LineString", "coordinates": l }),
            GeomValue::MultiLine(m) => json!({ "type": "MultiLineString", "coordinates": m }),
            GeomValue::Polygon(p) => json!({ "type": "Polygon", "coordinates": [p] }),
        }
    }

    /// Plain little-endian WKB — GeoParquet's `geometry` column encoding.
    pub fn to_wkb(&self) -> Vec<u8> {
        match self {
            GeomValue::Point(p) => wkb_point(p[0], p[1]),
            GeomValue::Line(l) => wkb_linestring(l),
            GeomValue::MultiLine(m) => wkb_multilinestring(m),
            GeomValue::Polygon(p) => wkb_polygon(std::slice::from_ref(p)),
        }
    }
}

fn wkb_point(lon: f64, lat: f64) -> Vec<u8> {
    let mut buf = Vec::with_capacity(1 + 4 + 16);
    buf.write_all(&[1u8]).unwrap();
    buf.write_all(&1u32.to_le_bytes()).unwrap();
    buf.write_all(&lon.to_le_bytes()).unwrap();
    buf.write_all(&lat.to_le_bytes()).unwrap();
    buf
}

fn wkb_linestring(coords: &[[f64; 2]]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(1 + 4 + 4 + 16 * coords.len());
    buf.write_all(&[1u8]).unwrap();
    buf.write_all(&2u32.to_le_bytes()).unwrap();
    buf.write_all(&(coords.len() as u32).to_le_bytes()).unwrap();
    for [lon, lat] in coords {
        buf.write_all(&lon.to_le_bytes()).unwrap();
        buf.write_all(&lat.to_le_bytes()).unwrap();
    }
    buf
}

fn wkb_polygon(rings: &[Vec<[f64; 2]>]) -> Vec<u8> {
    let total_points: usize = rings.iter().map(Vec::len).sum();
    let mut buf = Vec::with_capacity(1 + 4 + 4 + rings.len() * 4 + total_points * 16);
    buf.write_all(&[1u8]).unwrap();
    buf.write_all(&3u32.to_le_bytes()).unwrap();
    buf.write_all(&(rings.len() as u32).to_le_bytes()).unwrap();
    for ring in rings {
        buf.write_all(&(ring.len() as u32).to_le_bytes()).unwrap();
        for [lon, lat] in ring {
            buf.write_all(&lon.to_le_bytes()).unwrap();
            buf.write_all(&lat.to_le_bytes()).unwrap();
        }
    }
    buf
}

fn wkb_multilinestring(lines: &[Vec<[f64; 2]>]) -> Vec<u8> {
    let mut buf = Vec::new();
    buf.write_all(&[1u8]).unwrap();
    buf.write_all(&5u32.to_le_bytes()).unwrap(); // MultiLineString
    buf.write_all(&(lines.len() as u32).to_le_bytes()).unwrap();
    for line in lines {
        buf.extend_from_slice(&wkb_linestring(line));
    }
    buf
}

/// A `{table}_node_geom.bin`/`{table}_way_geom.bin`/`{table}_relation_geom.bin` reader consumed
/// forward, in step with `{table}.bin`'s own rows of that element kind — see `output::geojson`'s own
/// doc for why that's safe. One geometry table per kind (not per (kind, shape) pair — see
/// `GeomRow`'s own doc), so this single cursor type covers node/way/relation geometry alike; the
/// shape a given row decodes to comes from its own `geom_type` column, not which file it's in.
pub struct OrderedGeomCursor {
    reader: Option<StageReader<GeomRow>>,
    /// The next record read but not yet known to belong to the element being asked about — held
    /// across calls so a mismatch (this element has no entry) doesn't lose the record, which
    /// belongs to some later element.
    pending: Option<(i64, GeomValue)>,
    /// The most recently matched record, kept so a repeated `get` for the *same* `osm_id` (a
    /// side-split topic emits several tag rows per way, all sharing one geometry) reuses it instead
    /// of re-consuming the (already-advanced-past) cursor.
    last: Option<(i64, GeomValue)>,
}

impl OrderedGeomCursor {
    /// `None` if `path` doesn't exist — this topic declared no geometry output for this kind.
    pub fn open(path: &Path) -> anyhow::Result<Option<Self>> {
        Ok(StageReader::<GeomRow>::open_optional(path)?
            .map(|reader| OrderedGeomCursor { reader: Some(reader), pending: None, last: None }))
    }

    fn read_one(&mut self) -> anyhow::Result<Option<(i64, GeomValue)>> {
        loop {
            let Some(reader) = self.reader.as_mut() else { return Ok(None) };
            let Some(row) = reader.next_row()? else {
                self.reader = None; // exhausted — stop trying to read further
                return Ok(None);
            };
            if row.geom_ewkb.is_empty() {
                continue; // a way whose shape degenerated to nothing (see `WayGeometry`'s own doc)
            }
            let value = GeomValue::decode(row.geom_type, &row.geom_ewkb)?;
            return Ok(Some((row.osm_id, value)));
        }
    }

    /// This element's geometry, if the file has one — `None` otherwise. Must be called with
    /// `osm_id` in the same relative order `{table}.bin`'s same-kind rows come in (repeats for the
    /// same element, from side-split duplicates, are fine).
    pub fn get(&mut self, osm_id: i64) -> anyhow::Result<Option<&GeomValue>> {
        if let Some((id, _)) = &self.last {
            if *id == osm_id {
                return Ok(self.last.as_ref().map(|(_, g)| g));
            }
        }
        if self.pending.is_none() {
            self.pending = self.read_one()?;
        }
        match &self.pending {
            Some((id, _)) if *id == osm_id => {
                self.last = self.pending.take();
                Ok(self.last.as_ref().map(|(_, g)| g))
            }
            _ => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg(seg_idx: usize) -> EdgeGeom {
        EdgeGeom { seg_idx, coordinates: vec![[0.0, 0.0], [1.0, 1.0]] }
    }

    #[test]
    fn get_all_returns_contiguous_run_and_advances_past_it() {
        // way 1 has two segments, way 2 has one, way 3 has none in `edges` at all.
        let edges = vec![(1, seg(0)), (1, seg(1)), (2, seg(0))];
        let mut cursor = EdgeCursor::new(&edges);

        let way1 = cursor.get_all(1);
        assert_eq!(way1.len(), 2);
        assert_eq!(way1[0].seg_idx, 0);
        assert_eq!(way1[1].seg_idx, 1);

        // way 3 isn't in `edges` at all — no match, no advance, next call for way 2 still works.
        assert!(cursor.get_all(3).is_empty());

        let way2 = cursor.get_all(2);
        assert_eq!(way2.len(), 1);
        assert_eq!(way2[0].seg_idx, 0);

        // repeated call for the same way reuses `last` rather than re-scanning past it.
        assert_eq!(cursor.get_all(2).len(), 1);

        // a genuinely new id past the end of `edges` finds nothing.
        assert!(cursor.get_all(4).is_empty());
    }

    #[test]
    fn get_all_reuses_last_match_for_repeated_calls() {
        // A side-split topic asks for the same way's segments more than once in a row.
        let edges = vec![(1, seg(0)), (1, seg(1)), (2, seg(0))];
        let mut cursor = EdgeCursor::new(&edges);

        let first = cursor.get_all(1);
        assert_eq!(first.len(), 2);
        let repeat = cursor.get_all(1);
        assert_eq!(repeat.len(), 2);

        // cursor only advanced once — way 2 is still reachable after the repeat.
        let way2 = cursor.get_all(2);
        assert_eq!(way2.len(), 1);
    }

    #[test]
    fn get_all_on_empty_edges_always_returns_empty() {
        let edges: Vec<(i64, EdgeGeom)> = Vec::new();
        let mut cursor = EdgeCursor::new(&edges);
        assert!(cursor.get_all(1).is_empty());
        assert!(cursor.get_all(2).is_empty());
    }

    #[test]
    fn group_edges_by_way_preserves_seg_idx_order() {
        let edges = vec![(1, seg(0)), (1, seg(1)), (2, seg(0))];
        let grouped = group_edges_by_way(&edges);
        let way1 = grouped.get(&1).unwrap();
        assert_eq!(way1.len(), 2);
        assert_eq!(way1[0].seg_idx, 0);
        assert_eq!(way1[1].seg_idx, 1);
        assert_eq!(grouped.get(&2).unwrap().len(), 1);
        assert!(grouped.get(&3).is_none());
    }

    #[test]
    fn to_wkb_writes_plain_little_endian_wkb_with_no_srid_flag() {
        // Plain WKB (no SRID flag, unlike `geom::primitives`' EWKB writers) — byte 0 is the
        // endianness marker (1 = little-endian), bytes 1..5 the *unflagged* geometry type code.
        let point = GeomValue::Point([13.4, 52.5]);
        let wkb = point.to_wkb();
        assert_eq!(wkb[0], 1);
        assert_eq!(u32::from_le_bytes(wkb[1..5].try_into().unwrap()), 1); // Point
        assert_eq!(f64::from_le_bytes(wkb[5..13].try_into().unwrap()), 13.4);
        assert_eq!(f64::from_le_bytes(wkb[13..21].try_into().unwrap()), 52.5);

        let line = GeomValue::Line(vec![[0.0, 0.0], [1.0, 1.0]]);
        let wkb = line.to_wkb();
        assert_eq!(u32::from_le_bytes(wkb[1..5].try_into().unwrap()), 2); // LineString
        assert_eq!(u32::from_le_bytes(wkb[5..9].try_into().unwrap()), 2); // 2 points

        let multi = GeomValue::MultiLine(vec![vec![[0.0, 0.0], [1.0, 1.0]], vec![[2.0, 2.0], [3.0, 3.0]]]);
        let wkb = multi.to_wkb();
        assert_eq!(u32::from_le_bytes(wkb[1..5].try_into().unwrap()), 5); // MultiLineString
        assert_eq!(u32::from_le_bytes(wkb[5..9].try_into().unwrap()), 2); // 2 component lines

        let polygon = GeomValue::Polygon(vec![[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 0.0]]);
        let wkb = polygon.to_wkb();
        assert_eq!(u32::from_le_bytes(wkb[1..5].try_into().unwrap()), 3); // Polygon
        assert_eq!(u32::from_le_bytes(wkb[5..9].try_into().unwrap()), 1); // 1 ring
    }
}
