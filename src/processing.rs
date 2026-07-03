use crate::config::SplitMode;
use crate::engine::runner::build_geom_rows;
use crate::engine::topic_runner::TopicRunner;
use crate::osm::types::{OsmWay, WayData};
use crate::output::{
    geometry::{haversine_length_m, project_line},
    rows::{GeomRow, TopicRow},
    types::OsmMeta,
};
use crate::profile::{self, CLASSIFY, GEOMETRY};

/// Tag rows for one way: per-topic tag rows (index matches the runners slice) plus a bitmask of
/// which topics kept the way (produced ≥1 row). The mask gates geometry emission — geometry is
/// written only to the geom tables of topics that kept the way.
pub struct ClassifyOutput {
    pub topic_rows: Vec<Vec<TopicRow>>,
    pub mask: u32,
}

/// Tag-only classification for one way (Pass A). Builds `OsmMeta` once, then runs every topic's
/// pipeline against the way's raw tags. No geometry — coords are not needed and not available yet.
pub fn classify_way(runners: &[TopicRunner], wd: &WayData) -> ClassifyOutput {
    let meta = OsmMeta {
        updated_at: wd.meta.timestamp.and_then(|ts| {
            chrono::DateTime::from_timestamp(ts, 0)
                .map(|dt: chrono::DateTime<chrono::Utc>| dt.format("%Y-%m-%dT%H:%M:%SZ").to_string())
        }),
        updated_by: wd.meta.user.clone(),
        changeset_id: wd.meta.changeset,
    };

    profile::time(&CLASSIFY, || {
        let mut mask = 0u32;
        let topic_rows: Vec<Vec<TopicRow>> = runners
            .iter()
            .enumerate()
            .map(|(i, r)| {
                let rows = r.process(wd.id, &wd.tags, &meta);
                if !rows.is_empty() {
                    mask |= 1 << i;
                }
                rows
            })
            .collect();
        ClassifyOutput { topic_rows, mask }
    })
}

/// Build the geometry rows for a resolved way (geometry pass). Projects the line + measures length
/// once, then emits the variant rows per `split`. Topic-independent — the same rows are written to
/// each surviving topic's geom table.
pub fn geom_rows_for(way: &OsmWay, split: SplitMode) -> Vec<GeomRow> {
    profile::time(&GEOMETRY, || {
        let length_m = haversine_length_m(&way.coords);
        let geom = project_line(&way.coords);
        build_geom_rows(way, &geom, length_m, split)
    })
}
