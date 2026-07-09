//! Opt-in Pass-C sub-phase profiler. Enabled by `PASS_C_PROFILE=1` (set once at startup);
//! when off, `time` just runs the closure with a single relaxed atomic load and no clock read.
//!
//! Pass C's per-way work is spread across the reader (decode + tag build + geometry resolve) and
//! `processing` (`classify_way` + `geom_rows_for`: projection + classify), so the buckets are
//! filled from both. Accumulation is via relaxed atomics across the rayon threads — fine for
//! proportional attribution.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;

static ENABLED: AtomicBool = AtomicBool::new(false);

/// Per-sub-phase nanosecond accumulators (summed across all worker threads).
pub static DECODE: AtomicU64 = AtomicU64::new(0); // decode_block: inflate + protobuf parse
pub static TAGBUILD: AtomicU64 = AtomicU64::new(0); // way_data: build RawTags + refs + meta
pub static RESOLVE: AtomicU64 = AtomicU64::new(0); // resolve_way: coords lookup + geometry + cut_points
pub static GEOMETRY: AtomicU64 = AtomicU64::new(0); // haversine length + projection
pub static CLASSIFY: AtomicU64 = AtomicU64::new(0); // per-topic categorize + extract + row build
// Sub-split of CLASSIFY (transforms/side-split make up the remainder):
pub static CATEGORIZE: AtomicU64 = AtomicU64::new(0); // first-match category selection
pub static EXTRACT: AtomicU64 = AtomicU64::new(0); // field eval + const seeding + row build
pub static TAGCLONE: AtomicU64 = AtomicU64::new(0); // raw_tags.clone() per topic per element
pub static EXCLUDE: AtomicU64 = AtomicU64::new(0); // exclude_condition eval_filter
pub static PRECAT: AtomicU64 = AtomicU64::new(0); // unified pre_cat_steps (way-only, pre-categorize)
pub static SIDESPLIT: AtomicU64 = AtomicU64::new(0); // get_transformed_objects

/// Enable profiling if `PASS_C_PROFILE` is set in the environment. Call once at startup.
pub fn init_from_env() {
    if std::env::var_os("PASS_C_PROFILE").is_some() {
        ENABLED.store(true, Ordering::Relaxed);
    }
}

#[inline]
pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// Run `f`, adding its duration to `bucket` when profiling is on (otherwise zero overhead beyond
/// one atomic load).
#[inline]
pub fn time<T>(bucket: &AtomicU64, f: impl FnOnce() -> T) -> T {
    if !enabled() {
        return f();
    }
    let t = Instant::now();
    let r = f();
    bucket.fetch_add(t.elapsed().as_nanos() as u64, Ordering::Relaxed);
    r
}

/// RAII timer: adds the elapsed time to `bucket` when dropped (no-op if profiling is off). Use
/// when the region isn't a single closure — e.g. spans the rest of a loop body.
pub struct Scope(Option<(Instant, &'static AtomicU64)>);

impl Drop for Scope {
    fn drop(&mut self) {
        if let Some((t, b)) = self.0 {
            b.fetch_add(t.elapsed().as_nanos() as u64, Ordering::Relaxed);
        }
    }
}

#[inline]
pub fn scope(bucket: &'static AtomicU64) -> Scope {
    Scope(enabled().then(|| (Instant::now(), bucket)))
}

/// Log the accumulated Pass-C sub-phase totals (thread-summed CPU time, so it exceeds wall on a
/// multi-thread run — read the proportions, not the absolutes).
pub fn report() {
    if !enabled() {
        return;
    }
    let secs = |b: &AtomicU64| b.load(Ordering::Relaxed) as f64 / 1e9;
    let (d, t, r, g, c) = (secs(&DECODE), secs(&TAGBUILD), secs(&RESOLVE), secs(&GEOMETRY), secs(&CLASSIFY));
    let (cat, ext) = (secs(&CATEGORIZE), secs(&EXTRACT));
    let (clone, excl, precat, split) = (secs(&TAGCLONE), secs(&EXCLUDE), secs(&PRECAT), secs(&SIDESPLIT));
    let total = d + t + r + g + c;
    let pct = |x: f64| if total > 0.0 { 100.0 * x / total } else { 0.0 };
    let known = cat + ext + clone + excl + precat + split;
    tracing::info!(
        "[pass-c-profile] thread-summed CPU-s (share of Pass C work):\n\
         \tdecode   {:7.1}s  {:4.1}%\n\
         \ttag build{:7.1}s  {:4.1}%\n\
         \tresolve  {:7.1}s  {:4.1}%\n\
         \tgeometry {:7.1}s  {:4.1}%\n\
         \tclassify {:7.1}s  {:4.1}%  (categorize {:.1}s / extract {:.1}s /\n\
         \t                    tagclone {:.1}s / exclude {:.1}s / precat {:.1}s / sidesplit {:.1}s /\n\
         \t                    unaccounted {:.1}s)\n\
         \ttotal    {:7.1}s",
        d, pct(d), t, pct(t), r, pct(r), g, pct(g), c, pct(c),
        cat, ext, clone, excl, precat, split, (c - known).max(0.0), total
    );
}
