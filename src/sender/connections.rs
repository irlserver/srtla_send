use std::collections::HashSet;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::Result;
use smallvec::SmallVec;
use srtla_core::connection::SrtlaConnection;
use srtla_core::utils::now_ms;
use tracing::{debug, info, warn};

use super::sequence::SequenceTracker;
use super::uplink::{ConnIo, ConnIoMap};
use crate::net::{
    BatchUdpSocket, UplinkBinder, create_uplink_socket, resolve_remote, resolve_remote_all,
};

pub struct PendingConnectionChanges {
    pub new_ips: Option<SmallVec<IpAddr, 4>>,
    pub receiver_host: String,
    pub receiver_port: u16,
}

#[allow(clippy::too_many_arguments)]
pub async fn apply_connection_changes(
    connections: &mut SmallVec<SrtlaConnection, 4>,
    conn_io: &mut ConnIoMap,
    new_ips: &[IpAddr],
    receiver_host: &str,
    receiver_port: u16,
    last_selected_idx: &mut Option<usize>,
    seq_tracker: &mut SequenceTracker,
    binder: &Arc<dyn UplinkBinder>,
) {
    let current_labels: HashSet<String> = connections.iter().map(|c| c.label.clone()).collect();
    let desired_labels: HashSet<String> = new_ips
        .iter()
        .map(|ip| format!("{}:{} via {}", receiver_host, receiver_port, ip))
        .collect();

    // Remove stale connections
    let old_len = connections.len();
    let removed_conn_ids: Vec<u64> = connections
        .iter()
        .filter(|c| !desired_labels.contains(&c.label))
        .map(|c| c.conn_id)
        .collect();

    connections.retain(|c| desired_labels.contains(&c.label));

    // If connections were removed, reset selection state and clean up sequence tracker
    if connections.len() != old_len {
        info!("removed {} stale connections", old_len - connections.len());
        *last_selected_idx = None;

        // Remove entries for removed connections from the ring buffer and drop
        // their I/O half. Keyed by the stable conn_id, so this stays correct no
        // matter how `retain` shuffled the connections vec's indices.
        for conn_id in removed_conn_ids {
            seq_tracker.remove_connection(conn_id);
            conn_io.remove(&conn_id);
        }
    }

    // Add new connections
    let mut seen = HashSet::<IpAddr>::new();
    let new_ips_needed: SmallVec<IpAddr, 4> = new_ips
        .iter()
        .copied()
        .filter(|ip| seen.insert(*ip))
        .filter(|ip| {
            let label = format!("{}:{} via {}", receiver_host, receiver_port, ip);
            !current_labels.contains(&label)
        })
        .collect();

    if !new_ips_needed.is_empty() {
        let mut new_connections = create_connections_from_ips(
            &new_ips_needed,
            receiver_host,
            receiver_port,
            binder,
            conn_io,
        )
        .await;
        let added_count = new_connections.len();
        connections.append(&mut new_connections);

        if added_count > 0 {
            info!("added {} new connections", added_count);
        } else {
            warn!(
                "failed to add any new connections (attempted {})",
                new_ips_needed.len()
            );
        }
    }
}

pub async fn create_connections_from_ips(
    ips: &[IpAddr],
    receiver_host: &str,
    receiver_port: u16,
    binder: &Arc<dyn UplinkBinder>,
    conn_io: &mut ConnIoMap,
) -> SmallVec<SrtlaConnection, 4> {
    let mut connections = SmallVec::new();
    for ip in ips {
        match connect_uplink(*ip, receiver_host, receiver_port, binder).await {
            Ok((conn, io)) => {
                info!("added uplink {}", conn.label);
                conn_io.insert(conn.conn_id, io);
                connections.push(conn);
            }
            Err(e) => warn!(
                "failed to add uplink {} -> {}:{}: {}",
                ip, receiver_host, receiver_port, e
            ),
        }
    }
    connections
}

/// Open a UDP socket bound to `ip`, steered onto its egress by `binder`, and
/// pair it with a fresh socket-free [`SrtlaConnection`]. The connection and its
/// [`ConnIo`] share a `conn_id` so the shell can key the I/O map by it.
async fn connect_uplink(
    ip: IpAddr,
    receiver_host: &str,
    receiver_port: u16,
    binder: &Arc<dyn UplinkBinder>,
) -> Result<(SrtlaConnection, ConnIo)> {
    use rand::RngCore;

    let remote = resolve_remote(receiver_host, receiver_port).await?;
    let sock = create_uplink_socket(ip)?;
    binder.bind(&sock, ip)?;
    sock.set_nonblocking(true)?;
    let socket = Arc::new(BatchUdpSocket::new(sock, remote)?);

    let conn_id = rand::rng().next_u64();
    let label = format!("{}:{} via {}", receiver_host, receiver_port, ip);
    let conn = SrtlaConnection::new_registering(conn_id, label, ip, now_ms());
    let io = ConnIo {
        socket,
        binder: binder.clone(),
        remote,
    };
    Ok((conn, io))
}

/// Put a link into recovery and drop the sequence ownership it can no longer
/// answer for.
///
/// [`SrtlaConnection::mark_for_recovery`] is a pure method: it clears the link's
/// own `packet_log`, but it cannot reach the shell's [`SequenceTracker`], which
/// still maps every sequence this link queued to its `conn_id`. A NAK arriving
/// after the reset would then be attributed to — and shrink the window of — a
/// link that has been wiped and cannot be responsible for that loss. The two
/// halves belong together, so every recovery site calls this instead of
/// `mark_for_recovery` directly.
pub fn recover_connection(conn: &mut SrtlaConnection, seq_tracker: &mut SequenceTracker) {
    conn.mark_for_recovery();
    seq_tracker.remove_connection(conn.conn_id);
}

/// Re-open this uplink's socket in place (same egress binding and remote) and
/// reset the connection's protocol state. The pure state reset lives on
/// [`SrtlaConnection::reset_for_reconnect`]; only the socket work is here. The
/// caller must respawn the reader from the new `io.socket`.
pub async fn reconnect_uplink(
    conn: &mut SrtlaConnection,
    io: &mut ConnIo,
    receiver_host: &str,
    seq_tracker: &mut SequenceTracker,
    now: u64,
) -> Result<()> {
    let sock = create_uplink_socket(conn.local_ip)?;
    io.binder.bind(&sock, conn.local_ip)?;
    sock.set_nonblocking(true)?;
    io.socket = Arc::new(BatchUdpSocket::new(sock, io.remote)?);

    conn.reset_for_reconnect(now);
    // A reconnected link comes back with an empty packet log, so the tracker
    // must stop pointing already-queued sequences at it — same stale-ownership
    // problem `recover_connection` exists for, and a full socket reconnection is
    // the harder reset of the two.
    seq_tracker.remove_connection(conn.conn_id);
    // Don't reset connection_established_ms for reconnections — only set on REG3.
    conn.mark_reconnect_success();
    conn.reconnection.reset_startup_grace(now);

    spawn_receiver_dns_drift_check(receiver_host, io.remote, now);
    Ok(())
}

/// At most one DNS-drift warning per minute across the whole process. Every
/// bonded uplink reconnects on its own schedule, so a receiver that really has
/// moved would otherwise print one line per link per retry.
const DNS_DRIFT_WARN_INTERVAL_MS: u64 = 60_000;
static LAST_DNS_DRIFT_WARN_MS: AtomicU64 = AtomicU64::new(0);

/// Claim the next drift-warning slot, or report that one was used recently.
///
/// `0` means "never warned", so the first drift always speaks up. The claim is
/// a compare-exchange: two uplinks reconnecting in the same millisecond produce
/// one warning, not two.
fn claim_dns_drift_warning(now: u64) -> bool {
    loop {
        let last = LAST_DNS_DRIFT_WARN_MS.load(Ordering::Relaxed);
        if last != 0 && now.saturating_sub(last) < DNS_DRIFT_WARN_INTERVAL_MS {
            return false;
        }
        // `now.max(1)` keeps 0 reserved for "never warned" on a clock that
        // could legitimately read 0.
        match LAST_DNS_DRIFT_WARN_MS.compare_exchange_weak(
            last,
            now.max(1),
            Ordering::Relaxed,
            Ordering::Relaxed,
        ) {
            Ok(_) => return true,
            Err(_) => continue,
        }
    }
}

/// Has the receiver's hostname stopped answering with the address this uplink is
/// pinned to? An empty answer is not drift — it is a lookup that told us nothing.
fn dns_drift_detected(cached: SocketAddr, fresh: &[SocketAddr]) -> bool {
    !fresh.is_empty() && !fresh.contains(&cached)
}

/// Detect-only check that the receiver's hostname still resolves to the address
/// this uplink is pinned to.
///
/// This **never** swaps `io.remote`, not even when the re-resolution succeeds
/// and hands back a perfectly good new address. SRTLA registration binds the
/// whole bond to a receiver-generated connection ID (REG1/REG2/REG3): the ID
/// only means anything to the receiver instance that minted it. Repointing a
/// single uplink at a fresh DNS answer would split the bond across two receiver
/// identities — the new instance would not know our group, so that link would
/// end up worse off than it is talking to a stale address, while the rest of the
/// bond stayed where it was. Migrating the whole bond to a new receiver (tearing
/// down and re-registering every link together) is a separate future change;
/// until then the operator gets a warning and decides.
///
/// Runs detached so a slow or hanging resolver cannot delay the reconnect it was
/// triggered by — the reconnect is on the housekeeping tick of the main event
/// loop, and this is diagnostics.
fn spawn_receiver_dns_drift_check(receiver_host: &str, cached: SocketAddr, now: u64) {
    let host = receiver_host.to_string();
    tokio::spawn(async move {
        // A failed lookup is not a reconnect failure: DNS is often the first
        // thing to break when an uplink flaps, and the cached address is still
        // the best guess we have. Log at debug and stay put.
        match resolve_remote_all(&host, cached.port()).await {
            Ok(fresh) => {
                if dns_drift_detected(cached, &fresh) && claim_dns_drift_warning(now) {
                    let answers: SmallVec<String, 4> =
                        fresh.iter().map(|a| a.to_string()).collect();
                    warn!(
                        "receiver DNS drift: {host} no longer resolves to {cached} (now {}); \
                         keeping the current address because the bond is registered against this \
                         receiver instance",
                        answers.join(", ")
                    );
                }
            }
            Err(e) => debug!("could not re-resolve {host} on reconnect ({e}); keeping {cached}"),
        }
    });
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;

    use super::*;

    fn addr(last: u8) -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::new(203, 0, 113, last)), 5000)
    }

    #[test]
    fn dns_drift_needs_a_fresh_answer_that_excludes_the_cached_address() {
        // The receiver still answers with the address we are pinned to.
        assert!(!dns_drift_detected(addr(1), &[addr(1)]));
        // Multi-A record: ours is still one of them.
        assert!(!dns_drift_detected(addr(1), &[addr(2), addr(1)]));
        // Our address is gone from the answer — that is drift.
        assert!(dns_drift_detected(addr(1), &[addr(2)]));
        // An empty answer told us nothing; it must not be read as drift.
        assert!(!dns_drift_detected(addr(1), &[]));
    }

    #[test]
    fn dns_drift_warning_is_rate_limited_across_the_process() {
        // The static is process-wide, so drive it from a base far past any
        // stamp another test could have left, and never reset it.
        let base = LAST_DNS_DRIFT_WARN_MS
            .load(Ordering::Relaxed)
            .saturating_add(DNS_DRIFT_WARN_INTERVAL_MS * 10);

        assert!(claim_dns_drift_warning(base), "first drift must warn");
        assert!(
            !claim_dns_drift_warning(base + DNS_DRIFT_WARN_INTERVAL_MS - 1),
            "a second uplink reconnecting inside the window must stay quiet"
        );
        assert!(
            claim_dns_drift_warning(base + DNS_DRIFT_WARN_INTERVAL_MS),
            "the warning is due again once the window has passed"
        );
    }
}
