use std::collections::HashSet;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::Result;
use smallvec::SmallVec;
use srtla_core::connection::SrtlaConnection;
use srtla_core::registration::SrtlaRegistrationManager;
use srtla_core::utils::now_ms;
use tracing::{debug, info, warn};

use super::rehome::ReceiverResolver;
use super::sequence::SequenceTracker;
use super::uplink::{ConnIo, ConnIoMap};
use crate::net::{
    BatchUdpSocket, UplinkBinder, create_uplink_socket, resolve_remote_all,
    select_remote_for_source,
};

pub struct PendingConnectionChanges {
    pub new_ips: Option<SmallVec<IpAddr, 4>>,
    /// Normalised operator weight per entry of `new_ips` (same order). Empty =
    /// every link weight 1.
    pub new_weights: SmallVec<u8, 4>,
    pub receiver_host: String,
    pub receiver_port: u16,
}

#[allow(clippy::too_many_arguments)]
pub async fn apply_connection_changes(
    connections: &mut SmallVec<SrtlaConnection, 4>,
    conn_io: &mut ConnIoMap,
    new_ips: &[IpAddr],
    new_weights: &[u8],
    receiver_host: &str,
    receiver_port: u16,
    last_selected_idx: &mut Option<usize>,
    seq_tracker: &mut SequenceTracker,
    binder: &Arc<dyn UplinkBinder>,
    resolver: &dyn ReceiverResolver,
    reg: &mut SrtlaRegistrationManager,
) {
    // Taken before stale links are removed, so a reload that replaces every IP
    // still dials the receiver instance the bond is registered with.
    let pinned = pinned_remotes(connections, conn_io);
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
        reg.forget_connections(&removed_conn_ids, now_ms());
        for conn_id in removed_conn_ids {
            seq_tracker.remove_connection(conn_id);
            conn_io.remove(&conn_id);
        }
    }

    // Weight changes on the links that stay (a reload may only re-weight).
    apply_link_weights(connections, new_ips, new_weights);

    // Add new connections
    let mut seen = HashSet::<IpAddr>::new();
    let (new_ips_needed, new_weights_needed): (SmallVec<IpAddr, 4>, SmallVec<u8, 4>) = new_ips
        .iter()
        .copied()
        .filter(|ip| seen.insert(*ip))
        .filter(|ip| {
            let label = format!("{}:{} via {}", receiver_host, receiver_port, ip);
            !current_labels.contains(&label)
        })
        .map(|ip| (ip, weight_for(new_ips, new_weights, ip)))
        .unzip();

    if !new_ips_needed.is_empty() {
        let answers = reload_answers(
            &pinned,
            &new_ips_needed,
            resolver,
            receiver_host,
            receiver_port,
        )
        .await;
        let mut new_connections = create_connections_from_ips(
            &new_ips_needed,
            &new_weights_needed,
            &answers,
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

/// Receiver addresses for the uplinks a reload adds.
///
/// An added uplink joins the bond at the address the bond already uses in its
/// family, because registration ties the bond to one receiver instance. Only a
/// family the bond has no address in costs a lookup, at most one per reload.
/// A fresh answer contributes only addresses in such families. If the lookup
/// fails, those uplinks are not dialed.
async fn reload_answers(
    pinned: &[SocketAddr],
    added: &[IpAddr],
    resolver: &dyn ReceiverResolver,
    receiver_host: &str,
    receiver_port: u16,
) -> SmallVec<SocketAddr, 4> {
    let mut answers: SmallVec<SocketAddr, 4> = pinned.iter().copied().collect();
    if added
        .iter()
        .all(|ip| select_remote_for_source(pinned, *ip).is_ok())
    {
        return answers;
    }
    if let Some(fresh) = resolver
        .resolve(receiver_host.to_string(), receiver_port)
        .await
    {
        answers.extend(
            fresh
                .into_iter()
                .filter(|addr| select_remote_for_source(pinned, addr.ip()).is_err()),
        );
    }
    answers
}

/// Operator weight for `ip` from the parallel `ips`/`weights` lists (first
/// match wins); 1 when the list carries no weight for it.
pub fn weight_for(ips: &[IpAddr], weights: &[u8], ip: IpAddr) -> u8 {
    ips.iter()
        .position(|candidate| *candidate == ip)
        .and_then(|i| weights.get(i).copied())
        .unwrap_or(srtla_core::connection::LINK_WEIGHT_MIN)
}

/// Set every existing connection's operator weight from the parallel
/// `ips`/`weights` lists, logging each change. A link the lists do not name
/// goes back to 1.
pub fn apply_link_weights(connections: &mut [SrtlaConnection], ips: &[IpAddr], weights: &[u8]) {
    for conn in connections.iter_mut() {
        let weight = weight_for(ips, weights, conn.local_ip);
        if conn.link_weight != weight {
            info!(
                "uplink {} weight {} -> {}",
                conn.label, conn.link_weight, weight
            );
            conn.link_weight = weight;
        }
    }
}

pub async fn create_connections_from_ips(
    ips: &[IpAddr],
    weights: &[u8],
    answers: &[SocketAddr],
    receiver_host: &str,
    receiver_port: u16,
    binder: &Arc<dyn UplinkBinder>,
    conn_io: &mut ConnIoMap,
) -> SmallVec<SrtlaConnection, 4> {
    let mut connections = SmallVec::new();
    for ip in ips {
        let dialed = match select_remote_for_source(answers, *ip) {
            Ok(remote) => connect_uplink(*ip, remote, receiver_host, receiver_port, binder).await,
            Err(e) => Err(e),
        };
        match dialed {
            Ok((mut conn, io)) => {
                conn.link_weight = weight_for(ips, weights, *ip);
                info!("added uplink {} (weight {})", conn.label, conn.link_weight);
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
    remote: SocketAddr,
    receiver_host: &str,
    receiver_port: u16,
    binder: &Arc<dyn UplinkBinder>,
) -> Result<(SrtlaConnection, ConnIo)> {
    use rand::RngCore;

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
    rebuild_uplink_socket(conn, io, seq_tracker, now)?;
    spawn_receiver_dns_drift_check(receiver_host, io.remote, now);
    Ok(())
}

/// Re-open this uplink's socket against whatever `io.remote` currently holds and
/// reset the connection's protocol state.
///
/// Shared by the per-uplink reconnect above and the whole-bond re-home in
/// [`super::rehome`], which repoints `io.remote` for *every* uplink first and
/// then rebuilds each socket through here. Re-home does not want the drift check
/// bolted onto `reconnect_uplink`: it has just re-resolved the hostname itself,
/// so re-asking would be a wasted lookup that could only warn about the address
/// it deliberately moved to.
pub(super) fn rebuild_uplink_socket(
    conn: &mut SrtlaConnection,
    io: &mut ConnIo,
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
    conn.reconnection.reset_startup_grace(now);
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

/// Every receiver address the bond's uplinks currently send to, deduplicated,
/// in connection order.
pub(super) fn pinned_remotes(
    connections: &[SrtlaConnection],
    conn_io: &ConnIoMap,
) -> SmallVec<SocketAddr, 4> {
    let mut pinned = SmallVec::new();
    for conn in connections {
        if let Some(io) = conn_io.get(&conn.conn_id)
            && !pinned.contains(&io.remote)
        {
            pinned.push(io.remote);
        }
    }
    pinned
}

/// Bond-wide form of [`dns_drift_detected`], used by the re-home trigger.
///
/// True only when the hostname answered with something and *none* of the
/// addresses the bond is currently pinned to appear in that answer. Requiring
/// every uplink to have been dropped from DNS (rather than any one of them) is
/// the conservative reading: a GeoDNS or round-robin zone that still lists one
/// of our addresses has not moved the receiver, it has merely reordered its
/// answer, and re-homing on that would thrash the bond for nothing.
pub(super) fn receiver_moved(current: &[SocketAddr], fresh: &[SocketAddr]) -> bool {
    !current.is_empty() && current.iter().all(|c| dns_drift_detected(*c, fresh))
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
/// bond stayed where it was.
///
/// Migrating the *whole* bond together does exist — see [`super::rehome`] — but
/// it is deliberately not reachable from here. This check fires on a single
/// uplink's reconnect, which happens constantly on a healthy bond as individual
/// modems flap; tearing down a working stream because one link reconnected while
/// GeoDNS happened to answer differently would be far worse than the stale
/// address. Re-home only runs once the bond is *entirely* dead, where there is
/// nothing left to protect. On a healthy bond the operator still just gets this
/// warning and decides.
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

    #[tokio::test]
    async fn pinned_remotes_dedups_in_connection_order() {
        let connections = crate::test_helpers::create_test_connections(3).await;
        let mut conn_io = crate::test_helpers::create_test_conn_io_map(&connections);
        for (conn, remote) in connections.iter().zip([addr(2), addr(1), addr(2)]) {
            conn_io.get_mut(&conn.conn_id).unwrap().remote = remote;
        }

        assert_eq!(
            pinned_remotes(&connections, &conn_io).as_slice(),
            &[addr(2), addr(1)]
        );
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

    #[test]
    fn remote_selection_follows_the_source_family_in_answer_order() {
        let v4a = addr(1);
        let v4b = addr(2);
        let v6: SocketAddr = "[2001:db8::1]:5000".parse().unwrap();
        let answers = [v6, v4a, v4b];

        assert_eq!(
            crate::net::select_remote_for_source(&answers, v4a.ip()).unwrap(),
            v4a
        );
        assert_eq!(
            crate::net::select_remote_for_source(&answers, v6.ip()).unwrap(),
            v6
        );
        assert!(crate::net::select_remote_for_source(&[v4a], v6.ip()).is_err());
        assert!(crate::net::select_remote_for_source(&[v6], v4a.ip()).is_err());
        assert!(crate::net::select_remote_for_source(&[], v4a.ip()).is_err());
    }

    const RELOAD_HOST: &str = "rec.example.com";
    const RELOAD_PORT: u16 = 5000;

    fn bond_remote() -> SocketAddr {
        "127.0.0.1:7001".parse().unwrap()
    }

    /// Two IPv4 links pinned to [`bond_remote`], labelled so a reload against
    /// [`RELOAD_HOST`] keeps them.
    async fn pinned_bond() -> (SmallVec<SrtlaConnection, 4>, ConnIoMap) {
        let mut connections = crate::test_helpers::create_test_connections(2).await;
        let mut conn_io = crate::test_helpers::create_test_conn_io_map(&connections);
        for conn in connections.iter_mut() {
            conn.label = format!("{RELOAD_HOST}:{RELOAD_PORT} via {}", conn.local_ip);
            conn_io.get_mut(&conn.conn_id).unwrap().remote = bond_remote();
        }
        (connections, conn_io)
    }

    async fn reload(
        connections: &mut SmallVec<SrtlaConnection, 4>,
        conn_io: &mut ConnIoMap,
        new_ips: &[IpAddr],
        resolver: &dyn ReceiverResolver,
    ) {
        use crate::net::SourceIpBinder;

        let binder: Arc<dyn UplinkBinder> = Arc::new(SourceIpBinder);
        apply_connection_changes(
            connections,
            conn_io,
            new_ips,
            &[],
            RELOAD_HOST,
            RELOAD_PORT,
            &mut None,
            &mut SequenceTracker::new(),
            &binder,
            resolver,
            &mut SrtlaRegistrationManager::new(),
        )
        .await;
    }

    fn remote_via(
        connections: &[SrtlaConnection],
        conn_io: &ConnIoMap,
        ip: IpAddr,
    ) -> Option<SocketAddr> {
        connections
            .iter()
            .find(|c| c.local_ip == ip)
            .map(|c| conn_io[&c.conn_id].remote)
    }

    #[tokio::test]
    async fn reload_pin_dials_an_added_uplink_at_the_pinned_address() {
        let (mut connections, mut conn_io) = pinned_bond().await;
        let added: IpAddr = "127.0.0.1".parse().unwrap();
        let new_ips = [connections[0].local_ip, connections[1].local_ip, added];
        let resolver = super::super::rehome::StubResolver::new(vec![Some(vec![
            "127.0.0.1:9999".parse().unwrap(),
        ])]);

        reload(&mut connections, &mut conn_io, &new_ips, &*resolver).await;

        assert_eq!(connections.len(), 3);
        assert_eq!(
            remote_via(&connections, &conn_io, added),
            Some(bond_remote())
        );
        assert_eq!(resolver.calls(), 0, "a family the bond has needs no lookup");
    }

    #[tokio::test]
    async fn reload_pin_replacing_every_ip_stays_on_the_bond_receiver() {
        let (mut connections, mut conn_io) = pinned_bond().await;
        let added: IpAddr = "127.0.0.1".parse().unwrap();
        let resolver = super::super::rehome::StubResolver::new(vec![Some(vec![
            "127.0.0.1:9999".parse().unwrap(),
        ])]);

        reload(&mut connections, &mut conn_io, &[added], &*resolver).await;

        assert_eq!(connections.len(), 1);
        assert_eq!(connections[0].local_ip, added);
        assert_eq!(conn_io[&connections[0].conn_id].remote, bond_remote());
        assert_eq!(resolver.calls(), 0);
    }

    #[tokio::test]
    async fn reload_pin_looks_up_a_family_new_to_the_bond_once() {
        let (mut connections, mut conn_io) = pinned_bond().await;
        let kept = [connections[0].local_ip, connections[1].local_ip];
        let added: IpAddr = "::1".parse().unwrap();
        let resolver = super::super::rehome::StubResolver::new(vec![Some(vec![
            "127.0.0.1:9999".parse().unwrap(),
            "[::1]:9999".parse().unwrap(),
        ])]);

        reload(
            &mut connections,
            &mut conn_io,
            &[kept[0], kept[1], added],
            &*resolver,
        )
        .await;

        assert_eq!(connections.len(), 3);
        assert_eq!(
            remote_via(&connections, &conn_io, added),
            Some("[::1]:9999".parse().unwrap())
        );
        for ip in kept {
            assert_eq!(
                remote_via(&connections, &conn_io, ip),
                Some(bond_remote()),
                "the fresh IPv4 answer must not move the bond"
            );
        }
        assert_eq!(resolver.calls(), 1);
    }

    #[tokio::test]
    async fn reload_pin_skips_a_new_family_when_the_lookup_fails() {
        let (mut connections, mut conn_io) = pinned_bond().await;
        let kept_ids: Vec<u64> = connections.iter().map(|c| c.conn_id).collect();
        let kept = [connections[0].local_ip, connections[1].local_ip];
        let added: IpAddr = "::1".parse().unwrap();
        let resolver = super::super::rehome::StubResolver::new(vec![None]);

        reload(
            &mut connections,
            &mut conn_io,
            &[kept[0], kept[1], added],
            &*resolver,
        )
        .await;

        assert_eq!(
            connections.iter().map(|c| c.conn_id).collect::<Vec<_>>(),
            kept_ids
        );
        assert_eq!(remote_via(&connections, &conn_io, added), None);
        for ip in kept {
            assert_eq!(remote_via(&connections, &conn_io, ip), Some(bond_remote()));
        }
        assert_eq!(resolver.calls(), 1);
    }

    #[tokio::test]
    async fn startup_dials_only_sources_the_receiver_has_a_family_for() {
        use crate::net::SourceIpBinder;

        let binder: Arc<dyn UplinkBinder> = Arc::new(SourceIpBinder);
        let sources: [IpAddr; 2] = ["127.0.0.1".parse().unwrap(), "::1".parse().unwrap()];
        for receiver in ["127.0.0.1", "::1"] {
            let mut io = ConnIoMap::new();
            let answers = [SocketAddr::new(receiver.parse().unwrap(), 9000)];
            let connections = create_connections_from_ips(
                &sources,
                &[],
                &answers,
                receiver,
                9000,
                &binder,
                &mut io,
            )
            .await;
            assert_eq!(connections.len(), 1);
            assert_eq!(connections[0].local_ip, receiver.parse::<IpAddr>().unwrap());
            let remote = io[&connections[0].conn_id].remote;
            assert_eq!(remote.ip(), receiver.parse::<IpAddr>().unwrap());
        }
    }

    /// An IPv4-mapped IPv6 source (`::ffff:a.b.c.d`) is an IPv4 address in
    /// IPv6 notation. It gets an IPv4 socket and dials the receiver's IPv4
    /// answer, so a receiver with only an A record still takes it, and a
    /// reconnect rebuilds it the same way.
    #[tokio::test]
    async fn a_mapped_ipv4_source_dials_the_ipv4_answer() {
        use crate::net::SourceIpBinder;

        let binder: Arc<dyn UplinkBinder> = Arc::new(SourceIpBinder);
        let mapped: IpAddr = "::ffff:127.0.0.1".parse().unwrap();
        let v4: SocketAddr = "127.0.0.1:9000".parse().unwrap();
        let v6: SocketAddr = "[::1]:9000".parse().unwrap();
        assert_eq!(
            crate::net::select_remote_for_source(&[v6, v4], mapped).unwrap(),
            v4
        );

        let mut io = ConnIoMap::new();
        let connections =
            create_connections_from_ips(&[mapped], &[], &[v4], "127.0.0.1", 9000, &binder, &mut io)
                .await;
        assert_eq!(connections.len(), 1, "the mapped source must be dialed");
        let mut conn = connections.into_iter().next().unwrap();
        let entry = io.get_mut(&conn.conn_id).unwrap();
        assert_eq!(entry.remote, v4);

        rebuild_uplink_socket(&mut conn, entry, &mut SequenceTracker::new(), 100).unwrap();
        assert_eq!(entry.remote, v4);
    }

    /// The written form stays the link's identity, while the socket is bound to
    /// the plain IPv4 address and its datagrams really reach an IPv4-only
    /// receiver.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_mapped_ipv4_source_binds_ipv4_and_reaches_an_ipv4_receiver() {
        use crate::net::SourceIpBinder;

        let receiver = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let at = receiver.local_addr().unwrap();
        let binder: Arc<dyn UplinkBinder> = Arc::new(SourceIpBinder);
        let mapped: IpAddr = "::ffff:127.0.0.1".parse().unwrap();
        let mut io = ConnIoMap::new();
        let connections = create_connections_from_ips(
            &[mapped],
            &[],
            &[at],
            "127.0.0.1",
            at.port(),
            &binder,
            &mut io,
        )
        .await;
        assert_eq!(connections.len(), 1);
        assert_eq!(
            connections[0].local_ip, mapped,
            "the written form is the identity"
        );
        let entry = &io[&connections[0].conn_id];
        let local = entry.socket.get_ref().local_addr().unwrap();
        assert!(local.is_ipv4(), "bound as {local}");
        entry.socket.send(b"hello").await.unwrap();
        let mut buf = [0u8; 16];
        let (n, from) = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            receiver.recv_from(&mut buf),
        )
        .await
        .expect("the IPv4 receiver got nothing")
        .unwrap();
        assert_eq!(&buf[..n], b"hello");
        assert_eq!(from.ip(), "127.0.0.1".parse::<IpAddr>().unwrap());
    }

    /// A reload that adds a mapped source to an IPv4 bond pins it to the bond's
    /// IPv4 address without a lookup: the bond already has that family.
    #[tokio::test]
    async fn reload_pin_dials_a_mapped_ipv4_source_at_the_bond_address() {
        let (mut connections, mut conn_io) = pinned_bond().await;
        let added: IpAddr = "::ffff:127.0.0.1".parse().unwrap();
        let new_ips = [connections[0].local_ip, connections[1].local_ip, added];
        let resolver = super::super::rehome::StubResolver::new(vec![]);

        reload(&mut connections, &mut conn_io, &new_ips, &*resolver).await;

        assert_eq!(connections.len(), 3);
        assert_eq!(
            remote_via(&connections, &conn_io, added),
            Some(bond_remote())
        );
        assert_eq!(resolver.calls(), 0, "the bond has an IPv4 address already");
    }

    /// A reload that adds a mapped source to an IPv6-only bond looks up the
    /// family new to the bond once, and takes the A record even when the answer
    /// lists the AAAA record first.
    #[tokio::test]
    async fn reload_looks_up_ipv4_for_a_mapped_source_new_to_an_ipv6_bond() {
        let (mut connections, mut conn_io) = pinned_bond().await;
        connections.truncate(1);
        let v6_remote: SocketAddr = "[::1]:7001".parse().unwrap();
        connections[0].local_ip = "::1".parse().unwrap();
        connections[0].label = format!("{RELOAD_HOST}:{RELOAD_PORT} via ::1");
        let id = connections[0].conn_id;
        conn_io.retain(|k, _| *k == id);
        conn_io.get_mut(&id).unwrap().remote = v6_remote;
        let added: IpAddr = "::ffff:127.0.0.1".parse().unwrap();
        let resolver = super::super::rehome::StubResolver::new(vec![Some(vec![
            "[::1]:9999".parse().unwrap(),
            "127.0.0.1:9999".parse().unwrap(),
        ])]);
        let kept = connections[0].local_ip;

        reload(&mut connections, &mut conn_io, &[kept, added], &*resolver).await;

        assert_eq!(resolver.calls(), 1, "IPv4 is new to the bond");
        assert_eq!(
            remote_via(&connections, &conn_io, added),
            Some("127.0.0.1:9999".parse().unwrap())
        );
    }

    /// The failure family matching prevents: an IPv4 socket cannot send to an
    /// IPv6 receiver address at all.
    #[cfg(target_os = "linux")]
    #[test]
    fn ipv4_socket_cannot_reach_an_ipv6_receiver_address() {
        use crate::net::SourceIpBinder;

        let source: IpAddr = "127.0.0.1".parse().unwrap();
        let sock = create_uplink_socket(source).unwrap();
        SourceIpBinder.bind(&sock, source).unwrap();
        let v6: SocketAddr = "[::1]:9".parse().unwrap();
        let err = sock.send_to(b"x", &v6.into()).unwrap_err();
        assert_eq!(err.raw_os_error(), Some(libc::EAFNOSUPPORT));
    }
}
