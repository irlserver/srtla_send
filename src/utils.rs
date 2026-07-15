//! Utility functions shared across the codebase

use std::sync::OnceLock;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

/// Process-wide monotonic clock anchor.
///
/// `now_ms()` must never move backwards: every timeout, RTT sample, and
/// congestion-window deadline in this codebase is a difference between two
/// `now_ms()` reads (see the keepalive-echo RTT path in `connection::rtt`,
/// where `rtt = now_ms() - echoed_stamp` and *both* stamps are ours). A wall
/// clock (`SystemTime`) can step backwards on an NTP correction, which would
/// clamp an RTT to zero or falsely reset a link's timeout. `Instant` is
/// monotonic, so we anchor to it once and report `base_ms + elapsed`.
///
/// `base_ms` is captured from the wall clock at first read purely so the value
/// keeps an epoch-scale magnitude. Nothing depends on the absolute base (no
/// `now_ms()` value is interpreted by a peer or persisted), only on differences.
struct Clock {
    anchor: Instant,
    base_ms: u64,
}

fn clock() -> &'static Clock {
    static CLOCK: OnceLock<Clock> = OnceLock::new();
    CLOCK.get_or_init(|| Clock {
        anchor: Instant::now(),
        base_ms: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0),
    })
}

/// Monotonic time in milliseconds, anchored to an epoch-scale base.
///
/// Guaranteed non-decreasing within a process. Not a true wall clock: use it
/// only for measuring elapsed time between two reads, never as a timestamp to
/// compare against another machine's clock.
pub fn now_ms() -> u64 {
    let c = clock();
    c.base_ms + c.anchor.elapsed().as_millis() as u64
}
