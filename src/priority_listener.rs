//! Priority-sidecar UDP listener (I/O shell for [`srtla_core::priority`]).
//!
//! Consumes the 5-byte critical-window datagrams described in
//! [`srtla_core::priority`] off a dedicated loopback UDP socket and pushes the
//! derived deadlines into the shared [`CriticalWindow`]. Kept separate from the
//! pure `priority` state so that module carries no `tokio`/socket dependency.

use std::net::SocketAddr;

use tokio::net::UdpSocket;
use tracing::{info, trace, warn};

use srtla_core::priority::{CriticalWindow, DATAGRAM_LEN, PROTO_MAGIC};

/// Spawn a listener task that consumes priority datagrams from `bind_addr`
/// and pushes the derived deadlines into `state`. If `hub` is provided,
/// also publishes a `priority.window` event to subscribers on each
/// accepted datagram so downstream consumers can correlate priority
/// events with video keyframes in real time.
pub fn spawn_listener(
    bind_addr: SocketAddr,
    state: CriticalWindow,
    hub: Option<crate::subscriptions::SubscriptionHub>,
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
                        state.record_malformed();
                        trace!(?src, n, "dropped malformed priority datagram");
                        continue;
                    }
                    let window_ms = u32::from_be_bytes([buf[1], buf[2], buf[3], buf[4]]) as u64;
                    let now = srtla_core::utils::now_ms();
                    state.extend_to(now + window_ms);
                    trace!(window_ms, "critical window extended");
                    if let Some(ref hub) = hub {
                        hub.publish(
                            "priority.window",
                            serde_json::json!({
                                "at_ms": now,
                                "window_ms": window_ms,
                                "deadline_ms": now + window_ms,
                            }),
                        )
                        .await;
                    }
                }
                Err(e) => {
                    warn!(error = %e, "priority sidecar recv error");
                }
            }
        }
    })
}
