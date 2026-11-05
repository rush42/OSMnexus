//! Snapshot tests for the codegen backends (see `boundaries_plan.md`, step 9): any change to a
//! generated TS type shows up as a reviewable diff under `tests/snapshots/`. Update intentionally
//! changed snapshots with `cargo insta review` (or `INSTA_UPDATE=always cargo test`).

use osmnexus::codegen::ts::TsTypes;
use osmnexus::codegen::Backend;
use osmnexus::config_dir::Config;

fn snapshot_ts(config: &str) {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("configs").join(config);
    let cfg = Config::read(&root, false).expect("reading config");
    for name in cfg.sources.subdirs("").into_iter().filter(|n| !n.starts_with('_')) {
        // `0`: no decision trees needed, only the producers' field shapes.
        let topic = cfg.load(&name, 0).unwrap_or_else(|e| panic!("loading {config}/{name}: {e:#}"));
        insta::assert_snapshot!(format!("{config}__{name}"), TsTypes.generate(&name, &topic));
    }
}

/// Every config shipped in `configs/`, one test each so a failure names the config.
macro_rules! ts_snapshot_tests {
    ($($test:ident => $config:literal),* $(,)?) => {
        $(#[test] fn $test() { snapshot_ts($config); })*
    };
}

ts_snapshot_tests! {
    ts_types_tilda => "tilda",
    ts_types_public_transport => "public_transport",
    ts_types_trains => "trains",
    ts_types_osmnx => "osmnx",
    ts_types_idcoords => "idcoords",
    ts_types_live_raw => "live_raw",
}
