//! Criterion benchmarks for the per-element hot path (`build_topic_rows`), replacing the removed
//! `TILDA_PROFILE=1` in-process stage timers (see `src/topic/pipeline.rs`). Run with `cargo bench`.

use criterion::{black_box, criterion_group, criterion_main, Criterion};
use osmnexus::osm::types::{ElementKind, RawTags};
use osmnexus::osm::types::WayMeta;
use osmnexus::topic::pipeline::build_topic_rows;
use osmnexus::topic::runner::TopicRunner;

fn way_tags(pairs: &[(&str, &str)]) -> RawTags<'static> {
    pairs.iter().map(|&(k, v)| (k.to_owned().into(), v.to_owned().into())).collect()
}

fn bench_build_topic_rows(c: &mut Criterion) {
    let runner = TopicRunner::load("roads", 4).expect("load 'roads' topic from configs/tilda");
    let meta = WayMeta { timestamp: None, user: None, changeset: None };

    let tags = way_tags(&[
        ("highway", "residential"),
        ("name", "Example Street"),
        ("surface", "asphalt"),
        ("maxspeed", "30"),
        ("lit", "yes"),
        ("oneway", "no"),
    ]);

    c.bench_function("build_topic_rows/roads/residential_way", |b| {
        b.iter(|| build_topic_rows(black_box(&runner), ElementKind::Way, black_box(1), black_box(&tags), &meta))
    });
}

criterion_group!(benches, bench_build_topic_rows);
criterion_main!(benches);
