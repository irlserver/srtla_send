use std::collections::HashSet;
use std::net::IpAddr;
use std::sync::Arc;

use anyhow::Result;
use smallvec::SmallVec;
use srtla_core::connection::SrtlaConnection;
use srtla_core::utils::now_ms;
use tracing::{info, warn};

use super::sequence::SequenceTracker;
use super::uplink::{ConnIo, ConnIoMap};
use crate::net::{BatchUdpSocket, UplinkBinder, create_uplink_socket, resolve_remote};

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
    sock.connect(&remote.into())?;
    sock.set_nonblocking(true)?;
    let socket = Arc::new(BatchUdpSocket::new(sock)?);

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

/// Re-open this uplink's socket in place (same egress binding and remote) and
/// reset the connection's protocol state. The pure state reset lives on
/// [`SrtlaConnection::reset_for_reconnect`]; only the socket work is here. The
/// caller must respawn the reader from the new `io.socket`.
pub async fn reconnect_uplink(conn: &mut SrtlaConnection, io: &mut ConnIo, now: u64) -> Result<()> {
    let sock = create_uplink_socket(conn.local_ip)?;
    io.binder.bind(&sock, conn.local_ip)?;
    sock.connect(&io.remote.into())?;
    sock.set_nonblocking(true)?;
    io.socket = Arc::new(BatchUdpSocket::new(sock)?);

    conn.reset_for_reconnect(now);
    // Don't reset connection_established_ms for reconnections — only set on REG3.
    conn.mark_reconnect_success();
    conn.reconnection.reset_startup_grace(now);
    Ok(())
}
