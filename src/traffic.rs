//! Which side of the road traffic drives on — a process-global, set once at startup from
//! `--left-hand-traffic`, and read by the `directed` `Producer::Extract` mode
//! (`lang::producer`) to pick which physical side (`left`/`right`) maps to the way's
//! `forward`/`backward` direction. Right-hand traffic (the OSM/global default) is assumed unless
//! set.

use std::sync::atomic::{AtomicBool, Ordering};

static LEFT_HAND_TRAFFIC: AtomicBool = AtomicBool::new(false);

/// Set whether the import is for a left-hand-traffic region. Called once at startup from
/// `--left-hand-traffic`.
pub fn set_left_hand_traffic(left_hand: bool) {
    LEFT_HAND_TRAFFIC.store(left_hand, Ordering::Relaxed);
}

/// Whether the active import is for a left-hand-traffic region.
pub fn is_left_hand_traffic() -> bool {
    LEFT_HAND_TRAFFIC.load(Ordering::Relaxed)
}
