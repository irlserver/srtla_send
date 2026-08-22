//! Whole-bond re-home: move every uplink to a newly-resolved receiver address
//! and register from scratch.
//!
//! # Why the whole bond, or nothing
//!
//! SRTLA registration binds the bond to a receiver-generated connection id
//! (REG1 → REG2 → REG3). That id only means something to the receiver *instance*
//! that minted it, so repointing one uplink at a fresh DNS answer would split
//! the bond across two receiver identities — the moved link would arrive at an
//! instance that has never heard of our group and would be worse off than it was
//! talking to a stale address. The per-uplink reconnect path therefore never
//! swaps `io.remote` (it only warns; see [`super::connections`]). This module is
//! the one sanctioned place that address changes, and only because it changes it
//! for **every** uplink in the same housekeeping pass and then re-registers the
//! whole bond.
//!
//! # When it fires
//!
//! Conservatively, and only from the dead-bond branch of housekeeping. Every one
//! of these must hold:
//!
//! 1. Every uplink has timed out (`active_connections == 0`) *and* has stayed
//!    that way past the existing all-links-failed window
//!    ([`super::housekeeping::GLOBAL_TIMEOUT_MS`]) — the same timer that already
//!    decides the bond is unrecoverable, not a second parallel one. A bond with
//!    any live uplink is never re-homed.
//! 2. A fresh resolution of the receiver hostname succeeds *and* answers with
//!    none of the addresses the bond is pinned to. A failed or empty lookup is
//!    not drift, and a reordered multi-A answer that still lists one of our
//!    addresses is not drift either.
//! 3. The per-process rate limit allows it: at most one probe+re-home attempt
//!    per [`REHOME_MIN_INTERVAL_MS`].
//!
//! Nothing about the wire changes: the move is a plain RTT probe followed by the
//! standard REG1/REG2/REG3 flow, indistinguishable to the C reference receiver,
//! BELABOX, or irlserver `srtla_rec` from a sender that just started.
//!
//! # Composing with a SIGHUP IP reload
//!
//! Re-home rebuilds whatever uplinks the bond currently has, so it always runs
//! against the local-IP set that the last accepted reload installed. In the
//! other direction, a reload that lands *after* a re-home dials any newly added
//! uplink through [`crate::net::resolve_remote`], which picks the first answer —
//! the same deterministic pick this module makes — so the added link joins the
//! bond at the address the bond just moved to. Neither order can leave uplinks
//! split across two receivers.

use std::collections::HashMap;
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;

use smallvec::SmallVec;
use srtla_core::connection::SrtlaConnection;
use srtla_core::registration::SrtlaRegistrationManager;
use tokio::sync::mpsc::UnboundedSender;
use tokio::time::Duration;
use tracing::{debug, warn};

use super::connections::{rebuild_uplink_socket, receiver_moved, recover_connection};
use super::sequence::SequenceTracker;
use super::uplink::{ConnIoMap, ConnectionId, ReaderHandle, UplinkPacket, restart_reader_for};
use crate::net::resolve_remote_all;

/// At most one re-home *attempt* per minute, process-wide. The limit covers the
/// DNS probe as well as the migration: housekeeping runs every second, and a
/// dead bond would otherwise mean a receiver lookup every second for as long as
/// the outage lasts.
pub const REHOME_MIN_INTERVAL_MS: u64 = 60_000;

/// Ceiling on the re-home lookup. Housekeeping is the main event loop's tick, so
/// a resolver that hangs must not hang the sender with it. The bond is already
/// dead here, which is why blocking the tick at all is acceptable — but only
/// briefly.
const RESOLVE_TIMEOUT: Duration = Duration::from_secs(3);

/// Addresses a receiver hostname currently answers with.
pub type ResolvedAddrs = SmallVec<SocketAddr, 4>;

type ResolveFuture = Pin<Box<dyn Future<Output = Option<ResolvedAddrs>> + Send>>;

/// Re-resolution of the receiver hostname, behind a seam so the trigger policy
/// is testable without a live resolver.
pub trait ReceiverResolver: Send + Sync {
    /// `None` means the lookup failed, timed out, or answered with nothing. All
    /// three are explicitly **not** drift: they told us nothing about where the
    /// receiver is, so the bond stays where it is and keeps retrying normal
    /// reconnects.
    fn resolve(&self, host: String, port: u16) -> ResolveFuture;
}

/// The production resolver: a plain `getaddrinfo` under a timeout.
pub struct DnsResolver;

impl ReceiverResolver for DnsResolver {
    fn resolve(&self, host: String, port: u16) -> ResolveFuture {
        Box::pin(async move {
            match tokio::time::timeout(RESOLVE_TIMEOUT, resolve_remote_all(&host, port)).await {
                Ok(Ok(addrs)) if !addrs.is_empty() => Some(addrs),
                Ok(Ok(_)) => {
                    debug!("re-home lookup of {host} returned no addresses; staying put");
                    None
                }
                Ok(Err(e)) => {
                    debug!("re-home lookup of {host} failed ({e}); staying put");
                    None
                }
                Err(_) => {
                    warn!(
                        "re-home lookup of {host} timed out after {:?}; staying put",
                        RESOLVE_TIMEOUT
                    );
                    None
                }
            }
        })
    }
}

/// Trigger-policy state for whole-bond re-home: the opt-out switch, the rate
/// limit, and the resolver seam.
pub struct RehomeGate {
    enabled: bool,
    resolver: Arc<dyn ReceiverResolver>,
    /// When we last spent a probe slot. `None` = never, so the first dead bond
    /// gets its lookup immediately.
    last_probe_ms: Option<u64>,
    /// Completed migrations. Log-only telemetry: [`crate::stats::StatsSnapshot`]
    /// is a published JSON/Prometheus contract and this counter is not worth
    /// widening it for.
    rehomes: u64,
}

impl RehomeGate {
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled,
            resolver: Arc::new(DnsResolver),
            last_probe_ms: None,
            rehomes: 0,
        }
    }

    /// Claim this tick's probe slot, or report that the rate limit (or the
    /// opt-out) forbids one. Pure aside from the stamp it records.
    fn claim_probe(&mut self, now: u64) -> bool {
        if !self.enabled {
            return false;
        }
        if let Some(last) = self.last_probe_ms
            && now.saturating_sub(last) < REHOME_MIN_INTERVAL_MS
        {
            return false;
        }
        self.last_probe_ms = Some(now);
        true
    }
}

#[cfg(any(test, feature = "test-internals"))]
impl RehomeGate {
    /// Build a gate over a stub resolver, for tests that drive the trigger
    /// policy without DNS.
    pub fn with_resolver(enabled: bool, resolver: Arc<dyn ReceiverResolver>) -> Self {
        Self {
            enabled,
            resolver,
            last_probe_ms: None,
            rehomes: 0,
        }
    }

    pub fn rehome_count(&self) -> u64 {
        self.rehomes
    }
}

/// Test seam: answers a scripted queue of lookups and counts how many it was
/// asked for, so a test can assert not just the outcome but whether the trigger
/// policy even reached DNS.
#[cfg(any(test, feature = "test-internals"))]
pub struct StubResolver {
    answers: std::sync::Mutex<Vec<Option<ResolvedAddrs>>>,
    calls: std::sync::atomic::AtomicUsize,
}

#[cfg(any(test, feature = "test-internals"))]
impl StubResolver {
    /// `answers` are consumed in order; `None` models a failed or empty lookup.
    /// Once exhausted, every further lookup fails.
    pub fn new(answers: Vec<Option<Vec<SocketAddr>>>) -> Arc<Self> {
        Arc::new(Self {
            answers: std::sync::Mutex::new(
                answers
                    .into_iter()
                    .rev()
                    .map(|a| a.map(SmallVec::from_vec))
                    .collect(),
            ),
            calls: std::sync::atomic::AtomicUsize::new(0),
        })
    }

    pub fn calls(&self) -> usize {
        self.calls.load(std::sync::atomic::Ordering::Relaxed)
    }
}

#[cfg(any(test, feature = "test-internals"))]
impl ReceiverResolver for StubResolver {
    fn resolve(&self, _host: String, _port: u16) -> ResolveFuture {
        self.calls
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        // `pop` off a reversed vec = FIFO.
        let answer = self.answers.lock().unwrap().pop().flatten();
        Box::pin(async move { answer })
    }
}

/// Try to migrate the whole bond to a freshly-resolved receiver address.
///
/// Called only from the dead-bond branch of housekeeping, which supplies the
/// "every uplink has been down for longer than the all-failed window" half of
/// the trigger; this function supplies the rate limit and the DNS-drift half.
/// Returns `true` only when the bond was actually repointed, in which case the
/// caller re-arms the all-failed timer to give the fresh registration a full
/// window before the bond is declared unrecoverable.
#[allow(clippy::too_many_arguments)]
pub(super) async fn try_rehome(
    gate: &mut RehomeGate,
    connections: &mut [SrtlaConnection],
    conn_io: &mut ConnIoMap,
    reg: &mut SrtlaRegistrationManager,
    seq_tracker: &mut SequenceTracker,
    reader_handles: &mut HashMap<ConnectionId, ReaderHandle>,
    packet_tx: &UnboundedSender<UplinkPacket>,
    receiver_host: &str,
    dead_for_ms: u64,
    now: u64,
) -> bool {
    // Every address the bond is currently pinned to, deduplicated. Uplinks
    // resolve the hostname independently when they are first dialed, so in
    // principle they can sit on different answers.
    let mut current: ResolvedAddrs = SmallVec::new();
    for conn in connections.iter() {
        if let Some(io) = conn_io.get(&conn.conn_id)
            && !current.contains(&io.remote)
        {
            current.push(io.remote);
        }
    }
    let Some(&pinned) = current.first() else {
        return false;
    };

    if !gate.claim_probe(now) {
        return false;
    }

    let Some(fresh) = gate
        .resolver
        .resolve(receiver_host.to_string(), pinned.port())
        .await
    else {
        return false;
    };

    if !receiver_moved(&current, &fresh) {
        debug!(
            "bond down for {dead_for_ms}ms but {receiver_host} still resolves to it; not \
             re-homing, normal reconnects continue"
        );
        return false;
    }

    // Deterministic pick, matching `resolve_remote`: the first answer.
    let new_remote = fresh[0];
    warn!(
        "re-homing the whole bond: {receiver_host} no longer resolves to {pinned} and every \
         uplink has been down for {dead_for_ms}ms; moving {} uplink(s) from {pinned} to \
         {new_remote} and re-registering from scratch",
        connections.len()
    );

    rehome_bond(
        connections,
        conn_io,
        reg,
        seq_tracker,
        reader_handles,
        packet_tx,
        new_remote,
        now,
    )
    .await;
    gate.rehomes = gate.rehomes.saturating_add(1);
    true
}

/// Repoint every uplink at `new_remote` and restart registration.
///
/// All-or-nothing in the sense that matters: the remote swap happens for the
/// whole bond in one pass, before any socket is rebuilt, so an uplink whose
/// socket rebuild then fails is still coherently part of the migrated bond — the
/// ordinary reconnect path picks it up on a later tick and dials the *new*
/// address. The bond can never end up half on one receiver instance and half on
/// another.
#[allow(clippy::too_many_arguments)]
async fn rehome_bond(
    connections: &mut [SrtlaConnection],
    conn_io: &mut ConnIoMap,
    reg: &mut SrtlaRegistrationManager,
    seq_tracker: &mut SequenceTracker,
    reader_handles: &mut HashMap<ConnectionId, ReaderHandle>,
    packet_tx: &UnboundedSender<UplinkPacket>,
    new_remote: SocketAddr,
    now: u64,
) {
    // Pass 1 — the atomic part. Nothing here can fail.
    for conn in connections.iter() {
        if let Some(io) = conn_io.get_mut(&conn.conn_id) {
            io.remote = new_remote;
        }
    }

    // Pass 2 — rebuild each socket against the new remote, reset the link's
    // protocol state, drop its sequence-tracker ownership and respawn its
    // reader, exactly as the per-uplink reconnect does.
    for conn in connections.iter_mut() {
        match conn_io.get_mut(&conn.conn_id) {
            Some(io) => match rebuild_uplink_socket(conn, io, seq_tracker, now) {
                Ok(()) => restart_reader_for(conn, io.socket.clone(), reader_handles, packet_tx),
                Err(e) => {
                    warn!(
                        "{}: failed to rebuild socket for re-home ({e}); leaving it to the \
                         reconnect path, which will now dial {new_remote}",
                        conn.label
                    );
                    recover_connection(conn, seq_tracker);
                }
            },
            None => {
                warn!(
                    "{}: no I/O entry to re-home; marking for recovery",
                    conn.label
                );
                recover_connection(conn, seq_tracker);
            }
        }
    }

    // Pass 3 — start the handshake over. `reset_for_rehome` keeps our half of
    // the SRTLA id (so a receiver deriving its half deterministically reissues
    // the same full id and a load balancer keeps tracking one group) and wipes
    // everything else back to pre-REG1, including the probing state machine.
    // Re-probing then picks the REG1 target by RTT just like a fresh start.
    reg.reset_for_rehome();
    let probes = reg.start_probing(connections, now);
    for (idx, pkt) in probes {
        if let Some(conn) = connections.get(idx)
            && let Some(io) = conn_io.get(&conn.conn_id)
        {
            let _ = io.socket.send(&pkt).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr};

    use srtla_core::utils::now_ms;

    use super::*;
    use crate::sender::uplink::create_uplink_channel;
    use crate::test_helpers::{create_test_conn_io_map, create_test_connection};

    fn addr(last: u8) -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::new(203, 0, 113, last)), 5000)
    }

    struct Bond {
        connections: Vec<SrtlaConnection>,
        conn_io: ConnIoMap,
        reg: SrtlaRegistrationManager,
        seq_tracker: SequenceTracker,
        reader_handles: HashMap<ConnectionId, ReaderHandle>,
    }

    impl Bond {
        /// Two uplinks on 127.0.0.1 (so a socket rebuild really binds), both
        /// pinned to `remote`.
        async fn new(remote: SocketAddr) -> Self {
            let connections = vec![
                create_test_connection().await,
                create_test_connection().await,
            ];
            let mut conn_io = create_test_conn_io_map(&connections);
            for io in conn_io.values_mut() {
                io.remote = remote;
            }
            Self {
                connections,
                conn_io,
                reg: SrtlaRegistrationManager::new(),
                seq_tracker: SequenceTracker::new(),
                reader_handles: HashMap::new(),
            }
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

    async fn run_try_rehome(bond: &mut Bond, gate: &mut RehomeGate, now: u64) -> bool {
        let (packet_tx, _packet_rx) = create_uplink_channel();
        try_rehome(
            gate,
            &mut bond.connections,
            &mut bond.conn_io,
            &mut bond.reg,
            &mut bond.seq_tracker,
            &mut bond.reader_handles,
            &packet_tx,
            "rec.example.com",
            30_000,
            now,
        )
        .await
    }

    #[test]
    fn receiver_moved_needs_every_pinned_address_to_be_gone() {
        // Still listed: not a move.
        assert!(!receiver_moved(&[addr(1)], &[addr(1)]));
        // Multi-A reorder that still lists ours: not a move.
        assert!(!receiver_moved(&[addr(1)], &[addr(2), addr(1)]));
        // One uplink is still on a listed address: not a move for the bond.
        assert!(!receiver_moved(&[addr(1), addr(2)], &[addr(2), addr(3)]));
        // Every pinned address is gone: a move.
        assert!(receiver_moved(&[addr(1), addr(2)], &[addr(3)]));
        // An empty answer told us nothing.
        assert!(!receiver_moved(&[addr(1)], &[]));
        // Nothing pinned: nothing to move.
        assert!(!receiver_moved(&[], &[addr(1)]));
    }

    #[test]
    fn probe_slot_is_rate_limited_and_respects_the_opt_out() {
        let mut gate = RehomeGate::new(true);
        assert!(
            gate.claim_probe(1_000),
            "the first dead bond probes at once"
        );
        assert!(
            !gate.claim_probe(1_000 + REHOME_MIN_INTERVAL_MS - 1),
            "a second attempt inside the window must be refused"
        );
        assert!(
            gate.claim_probe(1_000 + REHOME_MIN_INTERVAL_MS),
            "the slot is due again once the window has passed"
        );

        let mut off = RehomeGate::new(false);
        assert!(!off.claim_probe(1_000), "--no-rehome must never probe");
    }

    #[tokio::test]
    async fn a_failed_lookup_is_not_drift() {
        let old = addr(1);
        let mut bond = Bond::new(old).await;
        let resolver = StubResolver::new(vec![None]);
        let mut gate = RehomeGate::with_resolver(true, resolver.clone());

        assert!(!run_try_rehome(&mut bond, &mut gate, now_ms()).await);
        assert_eq!(resolver.calls(), 1);
        assert_eq!(bond.remotes(), vec![old], "the bond must stay put");
        assert_eq!(gate.rehome_count(), 0);
    }

    #[tokio::test]
    async fn unchanged_dns_does_not_re_home() {
        let old = addr(1);
        let mut bond = Bond::new(old).await;
        // A reordered multi-A answer that still lists our address.
        let resolver = StubResolver::new(vec![Some(vec![addr(7), old])]);
        let mut gate = RehomeGate::with_resolver(true, resolver.clone());

        assert!(!run_try_rehome(&mut bond, &mut gate, now_ms()).await);
        assert_eq!(bond.remotes(), vec![old]);
        assert_eq!(gate.rehome_count(), 0);
    }

    #[tokio::test]
    async fn the_opt_out_skips_the_lookup_entirely() {
        let old = addr(1);
        let mut bond = Bond::new(old).await;
        let resolver = StubResolver::new(vec![Some(vec![addr(9)])]);
        let mut gate = RehomeGate::with_resolver(false, resolver.clone());

        assert!(!run_try_rehome(&mut bond, &mut gate, now_ms()).await);
        assert_eq!(resolver.calls(), 0, "--no-rehome must not even resolve");
        assert_eq!(bond.remotes(), vec![old]);
    }

    #[tokio::test]
    async fn drift_re_homes_once_then_is_rate_limited() {
        let old = addr(1);
        let new = addr(9);
        let mut bond = Bond::new(old).await;
        let resolver = StubResolver::new(vec![Some(vec![new]), Some(vec![new])]);
        let mut gate = RehomeGate::with_resolver(true, resolver.clone());

        let t0 = now_ms();
        assert!(run_try_rehome(&mut bond, &mut gate, t0).await);
        assert_eq!(bond.remotes(), vec![new], "every uplink moved together");
        assert_eq!(gate.rehome_count(), 1);

        // Still drifting (the stub would answer again), but inside the window.
        assert!(!run_try_rehome(&mut bond, &mut gate, t0 + 1_000).await);
        assert_eq!(resolver.calls(), 1, "the rate limit gates the lookup too");
        assert_eq!(gate.rehome_count(), 1);
    }

    #[tokio::test]
    async fn re_home_preserves_our_id_half_and_resets_to_pre_reg1() {
        let mut bond = Bond::new(addr(1)).await;

        // Model a bond that had fully registered against the old receiver: the
        // server half of the id is filled in, a REG3 grant is armed, a REG1
        // target is picked and the probing state machine has completed.
        let client_half: Vec<u8> = bond.reg.srtla_id()[..srtla_protocol::SRTLA_ID_LEN / 2].to_vec();
        bond.reg.srtla_id[srtla_protocol::SRTLA_ID_LEN / 2..].fill(0xab);
        bond.reg.arm_reg3_gate(0);
        bond.reg.set_reg1_target_idx(Some(1));
        bond.reg.set_pending_reg2_idx(Some(1));
        bond.reg.set_broadcast_reg2_pending(true);
        bond.reg.update_active_connections(&bond.connections);
        assert!(bond.reg.active_connections() > 0);

        let resolver = StubResolver::new(vec![Some(vec![addr(9)])]);
        let mut gate = RehomeGate::with_resolver(true, resolver);
        assert!(run_try_rehome(&mut bond, &mut gate, now_ms()).await);

        assert_eq!(
            &bond.reg.srtla_id()[..srtla_protocol::SRTLA_ID_LEN / 2],
            &client_half[..],
            "our half of the SRTLA id must survive the move so a deterministic receiver reissues \
             the same full id"
        );
        assert_ne!(
            &bond.reg.srtla_id()[srtla_protocol::SRTLA_ID_LEN / 2..],
            &[0xab; srtla_protocol::SRTLA_ID_LEN / 2][..],
            "the old receiver's half must be discarded, and re-randomized (not zeroed) so the \
             REG1 looks exactly like a fresh sender's"
        );

        assert_eq!(bond.reg.pending_reg2_idx(), None);
        assert!(!bond.reg.broadcast_reg2_pending());
        assert!(!bond.reg.is_awaiting_reg3(0), "REG3 grants must be revoked");
        assert_eq!(bond.reg.active_connections(), 0);
        assert!(
            bond.reg.is_probing(),
            "the bond must re-probe and then re-register from REG1"
        );
    }

    #[tokio::test]
    async fn re_home_clears_connection_and_seq_tracker_state() {
        let mut bond = Bond::new(addr(1)).await;

        // Give both links live-looking state and sequence-tracker ownership.
        for conn in bond.connections.iter_mut() {
            conn.connected = true;
            conn.in_flight_packets = 5;
        }
        let owner = bond.connections[0].conn_id;
        let t0 = now_ms();
        bond.seq_tracker.insert(1000, owner, t0);
        assert_eq!(bond.seq_tracker.get(1000, t0), Some(owner));

        let resolver = StubResolver::new(vec![Some(vec![addr(9)])]);
        let mut gate = RehomeGate::with_resolver(true, resolver);
        assert!(run_try_rehome(&mut bond, &mut gate, t0).await);

        assert_eq!(
            bond.seq_tracker.get(1000, t0),
            None,
            "a re-homed link must not stay the owner of sequences it can no longer answer for"
        );
        for conn in &bond.connections {
            assert!(!conn.connected, "{} must be unregistered", conn.label);
            assert_eq!(conn.in_flight_packets, 0);
        }
        assert_eq!(
            bond.reader_handles.len(),
            bond.connections.len(),
            "every re-homed uplink gets a fresh reader"
        );
    }
}
