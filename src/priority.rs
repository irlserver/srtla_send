//! Critical-packet priority sidecar.
//!
//! srtla_send's scheduler normally picks a link per packet by quality /
//! capacity / RTT. An upstream encoder that knows it is about to push a
//! keyframe (IDR / SPS / PPS burst) can open a short "critical window"
//! during which the scheduler routes packets to the highest-quality link
//! instead. This gives must-land video data the most reliable path at
//! the moment it matters most.
//!
//! The window is signalled over a dedicated UDP sidecar socket rather
//! than the JSON-RPC control channel. Same-host loopback UDP shares the
//! network stack path with the actual SRT data, so priority events are
//! ordered tightly against the packets they describe. The out-of-band
//! JSON-RPC socket, by contrast, could arrive microseconds late and miss
//! the earliest critical packets.
//!
//! ## Wire format
//!
//! One request per UDP datagram, 5 bytes fixed:
//!
//! ```text
//! byte 0 : 0xC1 — magic / version tag ("Critical v1")
//! bytes 1..5 : u32 big-endian — window length in milliseconds
//! ```
//!
//! srtla_send stores `now + window_ms` as the critical deadline.
//! `is_critical_now()` returns true while `now < deadline`. Overlapping
//! windows extend the deadline monotonically (fetch_max) so a fresh
//! hint can only ever push the deadline forward, never shrink it.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::net::UdpSocket;
use tracing::{info, trace, warn};

/// Magic byte identifying a priority-sidecar v1 datagram. Rejecting any
/// other leading byte lets us re-use the port for future framing later.
pub const PROTO_MAGIC: u8 = 0xC1;

/// Datagram length in bytes: `[magic u8][window_ms u32 big-endian]`.
pub const DATAGRAM_LEN: usize = 5;

/// Shared state reflecting the most recent critical-window deadline plus
/// observability counters. Cloned freely; all mutation is via atomics.
#[derive(Clone, Default)]
pub struct CriticalWindow {
    deadline_ms: Arc<AtomicU64>,
    windows_received: Arc<AtomicU64>,
    /// Set when a malformed datagram arrives. Surfaced in telemetry so a
    /// silently-dropped client becomes visible to operators.
    malformed_datagrams: Arc<AtomicU64>,
}

impl CriticalWindow {
    pub fn new() -> Self {
        Self::default()
    }

    /// Push the critical deadline forward (fetch_max). Ignores older
    /// deadlines, which keeps back-dated messages from shortening the
    /// active window.
    pub fn extend_to(&self, deadline_ms: u64) {
        self.deadline_ms.fetch_max(deadline_ms, Ordering::Relaxed);
        self.windows_received.fetch_add(1, Ordering::Relaxed);
    }

    /// Scheduler hot-path check. Cheap: one relaxed atomic load.
    #[inline]
    pub fn is_critical_now(&self, now_ms: u64) -> bool {
        self.deadline_ms.load(Ordering::Relaxed) > now_ms
    }

    pub fn windows_received(&self) -> u64 {
        self.windows_received.load(Ordering::Relaxed)
    }

    pub fn malformed_datagrams(&self) -> u64 {
        self.malformed_datagrams.load(Ordering::Relaxed)
    }

    /// Test-only: force a window from synchronous code without talking to
    /// the sidecar socket.
    #[cfg(test)]
    pub fn force_window(&self, deadline_ms: u64) {
        self.extend_to(deadline_ms);
    }
}

/// Spawn a listener task that consumes priority datagrams from `bind_addr`
/// and pushes the derived deadlines into `state`.
pub fn spawn_listener(
    bind_addr: SocketAddr,
    state: CriticalWindow,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let sock = match UdpSocket::bind(bind_addr).await {
            Ok(s) => s,
            Err(e) => {
                warn!(%bind_addr, error = %e, "failed to bind priority sidecar");
                return;
            }
        };
        let local = sock.local_addr().ok();
        info!(?local, "priority sidecar listening");

        let mut buf = [0u8; 16];
        loop {
            match sock.recv_from(&mut buf).await {
                Ok((n, src)) => {
                    if n != DATAGRAM_LEN || buf[0] != PROTO_MAGIC {
                        state
                            .malformed_datagrams
                            .fetch_add(1, Ordering::Relaxed);
                        trace!(?src, n, "dropped malformed priority datagram");
                        continue;
                    }
                    let window_ms =
                        u32::from_be_bytes([buf[1], buf[2], buf[3], buf[4]]) as u64;
                    let now = crate::utils::now_ms();
                    state.extend_to(now + window_ms);
                    trace!(window_ms, "critical window extended");
                }
                Err(e) => {
                    warn!(error = %e, "priority sidecar recv error");
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_critical_respects_deadline() {
        let w = CriticalWindow::new();
        assert!(!w.is_critical_now(100));
        w.force_window(500);
        assert!(w.is_critical_now(100));
        assert!(w.is_critical_now(499));
        assert!(!w.is_critical_now(500));
        assert!(!w.is_critical_now(501));
    }

    #[test]
    fn extend_to_is_monotonic() {
        let w = CriticalWindow::new();
        w.force_window(200);
        w.force_window(100); // older: ignored
        w.force_window(300); // newer: applied
        assert!(w.is_critical_now(250));
        assert!(w.is_critical_now(299));
        assert!(!w.is_critical_now(300));
        assert_eq!(w.windows_received(), 3);
    }
}
