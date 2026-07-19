//! Opt-in stage timing for the classify/geometry pipeline. Off by default (near-zero cost: one
//! relaxed atomic load per call site); enable with `TILDA_PROFILE=1` to accumulate per-stage
//! nanosecond totals + call counts across every rayon worker, printed once at the end of the run
//! via [`report`].
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;

static ENABLED: AtomicBool = AtomicBool::new(false);

pub fn init_from_env() {
    let on = std::env::var("TILDA_PROFILE").is_ok_and(|v| v != "0" && !v.is_empty());
    ENABLED.store(on, Ordering::Relaxed);
}

pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// One stage's running total: nanoseconds summed across every call, on every thread, plus a call
/// count for the average. `Relaxed` is enough — these are independent counters, not a lock.
#[derive(Default)]
pub struct Stage {
    nanos: AtomicU64,
    count: AtomicU64,
}

impl Stage {
    const fn new() -> Self {
        Self { nanos: AtomicU64::new(0), count: AtomicU64::new(0) }
    }

    fn record(&self, elapsed_nanos: u64) {
        self.nanos.fetch_add(elapsed_nanos, Ordering::Relaxed);
        self.count.fetch_add(1, Ordering::Relaxed);
    }

    fn avg_and_count(&self) -> (f64, u64) {
        let count = self.count.load(Ordering::Relaxed);
        let nanos = self.nanos.load(Ordering::Relaxed);
        let avg = if count == 0 { 0.0 } else { nanos as f64 / count as f64 };
        (avg, count)
    }
}

/// A running guard that records elapsed time into `stage` when dropped, but only does the
/// `Instant::now()` call at all when profiling is enabled (`enabled()` gate at each call site, not
/// here — see [`time`]).
pub struct Timer<'a> {
    stage: &'a Stage,
    start: Instant,
}

impl Drop for Timer<'_> {
    fn drop(&mut self) {
        self.stage.record(self.start.elapsed().as_nanos() as u64);
    }
}

/// Start timing `stage`, or return `None` when profiling is disabled (the caller's `if let`/`map`
/// then costs nothing beyond the one atomic load in [`enabled`]).
pub fn time(stage: &Stage) -> Option<Timer<'_>> {
    enabled().then(|| Timer { stage, start: Instant::now() })
}

pub static TAG_ENGINE: Stage = Stage::new();
pub static CATEGORIZE: Stage = Stage::new();
pub static GEOMETRY: Stage = Stage::new();
pub static ITERATION: Stage = Stage::new();

/// Log the accumulated per-stage averages, if profiling was enabled. A no-op otherwise.
pub fn report() {
    if !enabled() {
        return;
    }
    for (name, stage) in [
        ("tag engine (producer eval)", &TAG_ENGINE),
        ("categorize", &CATEGORIZE),
        ("geometry creation", &GEOMETRY),
        ("iteration (build_topic_rows)", &ITERATION),
    ] {
        let (avg, count) = stage.avg_and_count();
        tracing::info!("[profile] {name}: {count} calls, avg {avg:.0}ns/call, total {:.2}s", avg * count as f64 / 1e9);
    }
}
