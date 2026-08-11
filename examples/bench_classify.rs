//! Ad-hoc timing harness for the full select-phase classification path (stream_osm + real topic
//! classification), used to compare tag-allocation cost before/after the Cow-based `RawTags`
//! change. No DB/writer involved — tag rows are counted and dropped. Run with:
//!   cargo run --release --example bench_classify -- <path.osm.pbf> [--linear-classify]

use std::time::Instant;

use osmnexus::osm::reader::{stream_osm, Callbacks};
use osmnexus::osm::types::{NodeData, RelData, WayData};
use osmnexus::processing::{classify_node, classify_relation, classify_way};
use osmnexus::topic::TopicRunner;

fn main() -> anyhow::Result<()> {
    let path = std::env::args().nth(1).expect("usage: bench_classify <path.osm.pbf>");

    osmnexus::paths::set_config_root("configs/tilda");
    let t_init = Instant::now();
    let linear_classify = std::env::args().nth(2).as_deref() == Some("--linear-classify");
    let runners = TopicRunner::load_all(if linear_classify { 0 } else { 6 })?;
    println!("init (TopicRunner::load_all): {:.3}s", t_init.elapsed().as_secs_f64());
    let has_relations = runners.iter().any(|r| r.has_kind(osmnexus::osm::types::ElementKind::Relation));
    let has_nodes = runners.iter().any(|r| r.has_kind(osmnexus::osm::types::ElementKind::Node));

    let cb = Callbacks {
        has_relations,
        classify_rel: |rd: &RelData| -> Option<u32> {
            let rows = classify_relation(&runners, rd);
            let mut mask = 0u32;
            for (i, r) in rows.iter().enumerate() {
                if !r.is_empty() {
                    mask |= 1 << i;
                }
            }
            mask.ne(&0).then_some(mask)
        },
        classify_way: |wd: &WayData| -> Option<u32> {
            let out = classify_way(&runners, wd);
            (out.mask != 0).then_some(out.mask)
        },
        has_nodes,
        classify_node: |nd: &NodeData| -> bool {
            let rows = classify_node(&runners, nd);
            rows.iter().any(|r| !r.is_empty())
        },
        // This example measures classification only, so it opts out of everything the real
        // pipeline uses these for: no relation geometry, no graph output, and every node decoded
        // (an untagged-node skip would hide exactly the per-node cost being measured).
        relation_geom_mask: 0,
        skip_untagged_nodes: false,
        needs_graph: false,
    };

    let t = Instant::now();
    let ctx = stream_osm(&path, cb)?;
    let elapsed = t.elapsed();
    println!(
        "total: {:.2}s (node_coords={}, way_refs={}, rel_members={})",
        elapsed.as_secs_f64(),
        ctx.node_coords.iter().count(),
        ctx.way_refs.len(),
        ctx.rel_members.len(),
    );
    Ok(())
}
