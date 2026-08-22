use std::collections::HashMap;

use anyhow::{Result, anyhow};
use srtla_core::connection::{STARTUP_GRACE_MS, SrtlaConnection};
use srtla_core::registration::SrtlaRegistrationManager;
use tokio::sync::mpsc::UnboundedSender;
use tracing::{debug, error, info, warn};

use super::connections::{reconnect_uplink, recover_connection};
use super::rehome::{RehomeGate, try_rehome};
use super::sequence::SequenceTracker;
use super::uplink::{ConnIoMap, ConnectionId, ReaderHandle, UplinkPacket, restart_reader_for};

pub const GLOBAL_TIMEOUT_MS: u64 = 10_000;

/// Handle periodic housekeeping tasks.
///
/// With the ring buffer sequence tracker, we no longer need periodic cleanup
/// since old entries are naturally overwritten.
#[allow(clippy::too_many_arguments)]
pub async fn handle_housekeeping(
    connections: &mut [SrtlaConnection],
    conn_io: &mut ConnIoMap,
    reg: &mut SrtlaRegistrationManager,
    seq_tracker: &mut SequenceTracker,
    receiver_host: &str,
    classic: bool,
    now_ms: u64,
    all_failed_at: &mut Option<u64>,
    reader_handles: &mut HashMap<ConnectionId, ReaderHandle>,
    packet_tx: &UnboundedSender<UplinkPacket>,
    rehome: &mut RehomeGate,
) -> Result<()> {
    // If we're waiting on a REG2 response past the timeout, proactively retry REG1
    let current_ms = now_ms;
    let _ = reg.clear_pending_if_timed_out(current_ms);

    if reg.is_probing() {
        let was_probing = true;
        reg.check_probing_complete();
        // If probing just completed, reset grace period for the selected connection
        if !reg.is_probing()
            && was_probing
            && let Some(idx) = reg.get_selected_connection_idx()
            && let Some(conn) = connections.get_mut(idx)
        {
            conn.reconnection.startup_grace_deadline_ms = current_ms + STARTUP_GRACE_MS;
            debug!(
                "{}: Reset grace period after being selected for initial registration",
                conn.label
            );
        }
    }

    // housekeeping: drive registration, send keepalives
    for (i, conn) in connections.iter_mut().enumerate() {
        // Simple reconnect-on-timeout, then allow reg driver to proceed
        if conn.is_timed_out(current_ms) {
            if conn.should_attempt_reconnect(current_ms) {
                let label = conn.label.clone();
                conn.record_reconnect_attempt(current_ms);
                if conn.connection_established_ms() == 0 {
                    debug!("{} initial registration timed out; retrying", label);
                } else {
                    warn!("{} timed out; attempting full socket reconnection", label);
                }
                // Perform full socket reconnection (re-opens the socket in the
                // I/O map; the connection itself owns no socket).
                match conn_io.get_mut(&conn.conn_id) {
                    Some(io) => {
                        if let Err(e) =
                            reconnect_uplink(conn, io, receiver_host, seq_tracker, current_ms).await
                        {
                            warn!("{} failed to reconnect: {}", label, e);
                            // Fall back to recovery if reconnect fails
                            recover_connection(conn, seq_tracker);
                        } else {
                            restart_reader_for(conn, io.socket.clone(), reader_handles, packet_tx);
                        }
                    }
                    None => {
                        warn!("{} has no I/O entry; marking for recovery", label);
                        recover_connection(conn, seq_tracker);
                    }
                }

                match reg.pending_reg2_idx() {
                    Some(idx) if idx == i => {
                        info!("{} marked for recovery; re-sending REG1", label);
                        let pkt = reg.build_reg1_for(i, current_ms);
                        if let Some(io) = conn_io.get(&conn.conn_id) {
                            match io.socket.send(&pkt).await {
                                Ok(_) => conn.note_sent(current_ms),
                                Err(e) => warn!("Failed to send REG1 to uplink #{i}: {e:?}"),
                            }
                        }
                    }
                    Some(_) => {
                        debug!(
                            "{} timed out but another uplink is awaiting REG2; deferring",
                            label
                        );
                    }
                    None => {
                        info!("{} marked for recovery; re-sending REG2", label);
                        let pkt = reg.build_reg2(i);
                        if let Some(io) = conn_io.get(&conn.conn_id) {
                            match io.socket.send(&pkt).await {
                                Ok(_) => conn.note_sent(current_ms),
                                Err(e) => warn!("Failed to send REG2 to uplink #{i}: {e:?}"),
                            }
                        }
                    }
                }
            } else {
                debug!("{} timed out but in retry interval", conn.label);
            }
            continue;
        }

        if conn.needs_keepalive(current_ms) {
            let ka = conn.keepalive_packet(current_ms);
            if let Some(io) = conn_io.get(&conn.conn_id) {
                let _ = io.socket.send(&ka).await;
            }
        }
        if conn.needs_rtt_measurement(current_ms) {
            let ka = conn.keepalive_packet(current_ms);
            if let Some(io) = conn_io.get(&conn.conn_id) {
                let _ = io.socket.send(&ka).await;
            }
        }
        if !classic {
            conn.perform_window_recovery(current_ms);
        }
        // Update bitrate calculation
        conn.calculate_bitrate(current_ms);
        // Drive link lifecycle phase transitions
        conn.update_phase(current_ms);
        // Adapt the per-connection batch-send regime to the observed
        // load. Cheap; no-op when the regime hasn't changed.
        conn.recompute_batch_regime();

        // The reader task self-heals via CONN_TIMEOUT, but a task death
        // (panic / early return) would otherwise go undetected until that window.
        // Poll its handle cheaply each tick and respawn it for a still-active link
        // so inbound ACK/NAK/keepalive traffic resumes immediately, not seconds later.
        let reader_dead = reader_handles
            .get(&conn.conn_id)
            .is_some_and(|reader| reader.handle.is_finished());
        if reader_dead && let Some(io) = conn_io.get(&conn.conn_id) {
            warn!("{}: uplink reader task ended; restarting", conn.label);
            restart_reader_for(conn, io.socket.clone(), reader_handles, packet_tx);
        }
    }

    // Update active connections count (matches C implementation behavior)
    // C code resets active_connections=0 then counts non-timed-out connections
    reg.update_active_connections(connections);

    // drive registration (send REG1/REG2 as needed)
    let sends = reg.reg_driver_pending_sends(connections.len(), current_ms);
    if let Some((idx, pkt)) = sends.reg1
        && let Some(conn) = connections.get_mut(idx)
        && let Some(io) = conn_io.get(&conn.conn_id)
    {
        match io.socket.send(&pkt).await {
            Ok(_) => conn.note_sent(current_ms),
            Err(e) => warn!("Failed to send REG1 to uplink #{idx}: {e:?}"),
        }
    }
    if let Some(pkt) = sends.broadcast_reg2 {
        for (i, conn) in connections.iter_mut().enumerate() {
            if let Some(io) = conn_io.get(&conn.conn_id)
                && io.socket.send(&pkt).await.is_ok()
            {
                conn.note_sent(current_ms);
                debug!("REG2 → uplink #{i} sent");
            }
        }
    }

    // Check for connection failures and output appropriate error messages
    // This matches the C implementation's connection_housekeeping logic
    let active_connections = connections
        .iter()
        .filter(|c| !c.is_timed_out(current_ms))
        .count();

    if active_connections == 0 {
        if all_failed_at.is_none() {
            // Monotonic ms stamp on the single now_ms() clock; the all-links-failed
            // timeout below is a plain difference against the per-tick current_ms.
            *all_failed_at = Some(current_ms);
        }

        if reg.has_connected {
            error!("warning: no available connections");
        }

        // Timeout when all connections have failed. Measure elapsed-time-since-failure
        // so a transient all-down blip only trips after a full GLOBAL_TIMEOUT_MS of
        // sustained failure, not the instant uptime exceeds it.
        if let Some(failed_at) = all_failed_at
            && current_ms.saturating_sub(*failed_at) > GLOBAL_TIMEOUT_MS
        {
            // The bond is genuinely dead: every uplink is timed out and has
            // stayed that way for a full all-failed window. That, and only
            // that, is where a whole-bond re-home is allowed to consider
            // moving to a newly-resolved receiver address — see
            // `super::rehome` for why it is all-or-nothing and why a bond with
            // any live uplink is never touched. It also decides for itself
            // whether DNS actually drifted and whether the rate limit permits
            // an attempt; `false` means nothing changed and the existing
            // behaviour below stands.
            let dead_for_ms = current_ms.saturating_sub(*failed_at);
            if try_rehome(
                rehome,
                connections,
                conn_io,
                reg,
                seq_tracker,
                reader_handles,
                packet_tx,
                receiver_host,
                dead_for_ms,
                current_ms,
            )
            .await
            {
                // The bond now points somewhere new and is re-registering from
                // REG1. Re-arm the timer so the fresh handshake gets a full
                // window before the bond is declared unrecoverable again.
                *all_failed_at = Some(current_ms);
                return Ok(());
            }

            if reg.has_connected {
                error!("Failed to re-establish any connections");
                return Err(anyhow!("Failed to re-establish any connections"));
            } else {
                error!("Failed to establish any initial connections");
                return Err(anyhow!("Failed to establish any initial connections"));
            }
        }
    } else {
        *all_failed_at = None;
    }

    // NOTE: With the ring buffer sequence tracker, no cleanup is needed.
    // Old entries are naturally overwritten when the buffer wraps around.

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};

    use srtla_core::utils::now_ms;

    use super::*;
    use crate::sender::rehome::StubResolver;
    use crate::sender::uplink::{create_uplink_channel, sync_readers};
    use crate::test_helpers::{
        create_test_conn_io_map, create_test_connection, create_test_connections,
    };

    #[tokio::test]
    async fn dead_reader_is_restarted_for_active_connection() {
        let mut connections = vec![create_test_connection().await];
        let conn_id = connections[0].conn_id;
        let mut conn_io = create_test_conn_io_map(&connections);
        let mut reg = SrtlaRegistrationManager::new();
        let mut all_failed_at: Option<u64> = None;
        let mut seq_tracker = SequenceTracker::new();

        let (packet_tx, _packet_rx) = create_uplink_channel();
        let mut reader_handles: HashMap<ConnectionId, ReaderHandle> = HashMap::new();
        sync_readers(&connections, &conn_io, &mut reader_handles, &packet_tx);

        // Abort the reader and let the runtime drive cancellation to completion,
        // reproducing a silently dead task (a handle that reports is_finished()).
        reader_handles.get(&conn_id).unwrap().handle.abort();
        for _ in 0..1000 {
            if reader_handles.get(&conn_id).unwrap().handle.is_finished() {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(
            reader_handles.get(&conn_id).unwrap().handle.is_finished(),
            "reader task should be dead after abort"
        );

        handle_housekeeping(
            &mut connections,
            &mut conn_io,
            &mut reg,
            &mut seq_tracker,
            "127.0.0.1",
            false,
            now_ms(),
            &mut all_failed_at,
            &mut reader_handles,
            &packet_tx,
            &mut RehomeGate::new(false),
        )
        .await
        .expect("housekeeping on an active connection must not fail");

        // A finished handle can never un-finish itself; a live handle proves
        // housekeeping spawned a fresh reader in its place.
        assert!(
            !reader_handles.get(&conn_id).unwrap().handle.is_finished(),
            "housekeeping must respawn the dead reader for the still-active connection"
        );
    }

    /// The all-uplinks-failed timeout must measure time *since* the links failed,
    /// not the uptime captured at the moment of failure. With the buggy
    /// uptime-at-failure measure, the timer tripped on the first all-down pass as
    /// soon as total uptime exceeded `GLOBAL_TIMEOUT_MS`, erroring on a transient
    /// blip. Arming and the first re-check must not error; only a full
    /// `GLOBAL_TIMEOUT_MS` of sustained failure may fire it.
    ///
    /// `handle_housekeeping` takes `now` as an argument, so the elapsed-since-
    /// failure window is driven by the explicit timestamps passed here — no tokio
    /// virtual clock. `now_ms()` is monotonic and not tokio-controlled, so a paused
    /// clock could not drive this anymore.
    #[tokio::test]
    async fn all_failed_timeout_measures_elapsed_since_failure() {
        let mut connections = create_test_connections(2).await;
        let mut conn_io = create_test_conn_io_map(&connections);
        let mut reg = SrtlaRegistrationManager::new();
        // Models a stream that was established and then lost every link.
        reg.has_connected = true;
        let mut reader_handles: HashMap<ConnectionId, ReaderHandle> = HashMap::new();
        let (packet_tx, _packet_rx) = tokio::sync::mpsc::unbounded_channel::<UplinkPacket>();
        let mut all_failed_at: Option<u64> = None;
        let mut seq_tracker = SequenceTracker::new();

        let t0 = now_ms();

        // Drop all uplinks. Pin the reconnect backoff well past the whole test
        // window (max failure count -> 120s backoff) so housekeeping reaches the
        // timeout branch instead of attempting a socket reconnection.
        for conn in connections.iter_mut() {
            conn.mark_for_recovery();
            conn.reconnection.last_reconnect_attempt_ms = t0;
            conn.reconnection.reconnect_failure_count = 5;
        }

        // Arm: first all-down pass. Uptime is irrelevant (only now - failed_at
        // matters), so arming must not error however long the process has run.
        let armed = handle_housekeeping(
            &mut connections,
            &mut conn_io,
            &mut reg,
            &mut seq_tracker,
            "127.0.0.1",
            false,
            t0,
            &mut all_failed_at,
            &mut reader_handles,
            &packet_tx,
            &mut RehomeGate::new(false),
        )
        .await;
        assert!(armed.is_ok(), "arming the all-failed timer must not error");
        assert!(all_failed_at.is_some(), "the failure timer should be armed");

        // Still within the window: no error until a full GLOBAL_TIMEOUT_MS elapses.
        let within = handle_housekeeping(
            &mut connections,
            &mut conn_io,
            &mut reg,
            &mut seq_tracker,
            "127.0.0.1",
            false,
            t0 + GLOBAL_TIMEOUT_MS - 1000,
            &mut all_failed_at,
            &mut reader_handles,
            &packet_tx,
            &mut RehomeGate::new(false),
        )
        .await;
        assert!(
            within.is_ok(),
            "no error until a full {GLOBAL_TIMEOUT_MS}ms has elapsed since the links failed"
        );

        // Past the window: fires.
        let fired = handle_housekeeping(
            &mut connections,
            &mut conn_io,
            &mut reg,
            &mut seq_tracker,
            "127.0.0.1",
            false,
            t0 + GLOBAL_TIMEOUT_MS + 1000,
            &mut all_failed_at,
            &mut reader_handles,
            &packet_tx,
            &mut RehomeGate::new(false),
        )
        .await;
        assert!(
            fired.is_err(),
            "the all-failed timeout must fire once a full {GLOBAL_TIMEOUT_MS}ms has elapsed since \
             failure"
        );
    }

    /// A bond in the shape the re-home trigger cares about: uplinks on
    /// 127.0.0.1 (so a socket rebuild really binds), all pinned to `remote`.
    struct RehomeFixture {
        connections: Vec<SrtlaConnection>,
        conn_io: ConnIoMap,
        reg: SrtlaRegistrationManager,
        seq_tracker: SequenceTracker,
        reader_handles: HashMap<ConnectionId, ReaderHandle>,
        all_failed_at: Option<u64>,
    }

    impl RehomeFixture {
        async fn new(remote: SocketAddr) -> Self {
            let connections = vec![
                create_test_connection().await,
                create_test_connection().await,
            ];
            let mut conn_io = create_test_conn_io_map(&connections);
            for io in conn_io.values_mut() {
                io.remote = remote;
            }
            let mut reg = SrtlaRegistrationManager::new();
            // Models a stream that was established and then lost, so the
            // all-failed branch takes the "re-establish" path.
            reg.has_connected = true;
            Self {
                connections,
                conn_io,
                reg,
                seq_tracker: SequenceTracker::new(),
                reader_handles: HashMap::new(),
                all_failed_at: None,
            }
        }

        /// Keep every uplink live as of `now` (fresh delivery proof, grace
        /// window open), so the bond reads as healthy however far the test
        /// clock has advanced.
        fn keep_all_uplinks_live(&mut self, now: u64) {
            for conn in self.connections.iter_mut() {
                conn.connected = true;
                conn.last_received = Some(now);
                conn.reconnection.startup_grace_deadline_ms = now + STARTUP_GRACE_MS;
            }
        }

        /// Drop every uplink and pin the reconnect backoff past the test window
        /// so housekeeping reaches the all-failed branch rather than spending
        /// the tick on per-link socket reconnections.
        fn kill_all_uplinks(&mut self, at: u64) {
            for conn in self.connections.iter_mut() {
                conn.mark_for_recovery();
                conn.reconnection.last_reconnect_attempt_ms = at;
                conn.reconnection.reconnect_failure_count = 5;
            }
        }

        async fn tick(&mut self, gate: &mut RehomeGate, now: u64) -> Result<()> {
            let (packet_tx, _packet_rx) = create_uplink_channel();
            handle_housekeeping(
                &mut self.connections,
                &mut self.conn_io,
                &mut self.reg,
                &mut self.seq_tracker,
                "rec.example.com",
                false,
                now,
                &mut self.all_failed_at,
                &mut self.reader_handles,
                &packet_tx,
                gate,
            )
            .await
        }

        fn remotes(&self) -> Vec<SocketAddr> {
            let mut remotes: Vec<SocketAddr> = self
                .connections
                .iter()
                .map(|c| self.conn_io.get(&c.conn_id).unwrap().remote)
                .collect();
            remotes.sort();
            remotes.dedup();
            remotes
        }
    }

    fn remote(last: u8) -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::new(203, 0, 113, last)), 5000)
    }

    /// The most important half of the policy: a bond with a live uplink is never
    /// re-homed, however far the receiver's DNS has drifted. The lookup must not
    /// even happen — GeoDNS answers change constantly on a healthy bond.
    #[tokio::test]
    async fn a_healthy_bond_is_never_re_homed_however_much_dns_drifts() {
        let old = remote(1);
        let mut fixture = RehomeFixture::new(old).await;
        let resolver = StubResolver::new(vec![Some(vec![remote(9)])]);
        let mut gate = RehomeGate::with_resolver(true, resolver.clone());

        // Both uplinks stay live, so the all-failed timer never arms.
        let t0 = now_ms();
        for now in [t0, t0 + GLOBAL_TIMEOUT_MS * 3] {
            fixture.keep_all_uplinks_live(now);
            fixture
                .tick(&mut gate, now)
                .await
                .expect("a healthy bond must not error");
        }

        assert_eq!(resolver.calls(), 0, "a live bond must not even re-resolve");
        assert_eq!(fixture.remotes(), vec![old]);
        assert_eq!(gate.rehome_count(), 0);
        assert!(fixture.all_failed_at.is_none());
    }

    /// A dead bond alone is not enough: the receiver hostname must actually have
    /// stopped answering with the address we are pinned to. Otherwise the
    /// ordinary all-failed error path stands and normal reconnects continue.
    #[tokio::test]
    async fn a_dead_bond_with_unchanged_dns_is_not_re_homed() {
        let old = remote(1);
        let mut fixture = RehomeFixture::new(old).await;
        // Answer with a reordered multi-A set that still lists our address.
        let resolver = StubResolver::new(vec![Some(vec![remote(7), old])]);
        let mut gate = RehomeGate::with_resolver(true, resolver.clone());

        let t0 = now_ms();
        fixture.kill_all_uplinks(t0);
        fixture
            .tick(&mut gate, t0)
            .await
            .expect("arming must not error");
        let fired = fixture
            .tick(&mut gate, t0 + GLOBAL_TIMEOUT_MS + 1_000)
            .await;

        assert_eq!(resolver.calls(), 1, "the probe runs, and answers 'no move'");
        assert_eq!(fixture.remotes(), vec![old], "the bond must stay put");
        assert_eq!(gate.rehome_count(), 0);
        assert!(
            fired.is_err(),
            "without drift the pre-existing all-failed error must still fire"
        );
    }

    /// A dead bond must not be re-homed before the existing all-failed window
    /// has elapsed — a blip where every modem drops for a second or two is
    /// exactly what that window exists to absorb.
    #[tokio::test]
    async fn a_dead_bond_inside_the_all_failed_window_is_not_probed() {
        let old = remote(1);
        let mut fixture = RehomeFixture::new(old).await;
        let resolver = StubResolver::new(vec![Some(vec![remote(9)])]);
        let mut gate = RehomeGate::with_resolver(true, resolver.clone());

        let t0 = now_ms();
        fixture.kill_all_uplinks(t0);
        fixture
            .tick(&mut gate, t0)
            .await
            .expect("arming must not error");
        fixture
            .tick(&mut gate, t0 + GLOBAL_TIMEOUT_MS - 1_000)
            .await
            .expect("inside the window nothing fires");

        assert_eq!(resolver.calls(), 0, "no probe inside the all-failed window");
        assert_eq!(fixture.remotes(), vec![old]);
    }

    /// The whole trigger, end to end: dead past the window plus real drift moves
    /// every uplink together, re-arms the all-failed timer instead of erroring,
    /// and is then rate-limited even though DNS still says the receiver moved.
    #[tokio::test]
    async fn a_dead_bond_with_drift_re_homes_once_then_is_rate_limited() {
        let old = remote(1);
        let new = remote(9);
        let mut fixture = RehomeFixture::new(old).await;
        let resolver = StubResolver::new(vec![Some(vec![new]), Some(vec![new])]);
        let mut gate = RehomeGate::with_resolver(true, resolver.clone());

        let t0 = now_ms();
        fixture.kill_all_uplinks(t0);
        fixture
            .tick(&mut gate, t0)
            .await
            .expect("arming must not error");

        let moved_at = t0 + GLOBAL_TIMEOUT_MS + 1_000;
        fixture
            .tick(&mut gate, moved_at)
            .await
            .expect("a successful re-home replaces the all-failed error");

        assert_eq!(fixture.remotes(), vec![new], "every uplink moved together");
        assert_eq!(gate.rehome_count(), 1);
        assert_eq!(
            fixture.all_failed_at,
            Some(moved_at),
            "the fresh registration gets a full window before the bond is declared unrecoverable \
             again"
        );
        assert!(
            fixture.reg.is_probing(),
            "the bond re-registers from scratch, starting with RTT probing"
        );

        // Still dead and still drifting, but inside the rate-limit window.
        fixture.kill_all_uplinks(moved_at);
        let fired = fixture
            .tick(&mut gate, moved_at + GLOBAL_TIMEOUT_MS + 1_000)
            .await;
        assert_eq!(resolver.calls(), 1, "the rate limit gates the lookup too");
        assert_eq!(gate.rehome_count(), 1);
        assert!(fired.is_err(), "the all-failed error path stands meanwhile");
    }

    /// `--no-rehome` restores the pre-existing behaviour exactly.
    #[tokio::test]
    async fn the_opt_out_leaves_a_dead_drifted_bond_where_it_is() {
        let old = remote(1);
        let mut fixture = RehomeFixture::new(old).await;
        let resolver = StubResolver::new(vec![Some(vec![remote(9)])]);
        let mut gate = RehomeGate::with_resolver(false, resolver.clone());

        let t0 = now_ms();
        fixture.kill_all_uplinks(t0);
        fixture
            .tick(&mut gate, t0)
            .await
            .expect("arming must not error");
        let fired = fixture
            .tick(&mut gate, t0 + GLOBAL_TIMEOUT_MS + 1_000)
            .await;

        assert_eq!(resolver.calls(), 0);
        assert_eq!(fixture.remotes(), vec![old]);
        assert!(
            fired.is_err(),
            "the pre-existing all-failed error must fire"
        );
    }
}
