use std::net::SocketAddr;

use anyhow::Result;
use smallvec::SmallVec;
use tokio::net::UdpSocket;
use tracing::debug;

use srtla_core::connection::{SrtlaConnection, SrtlaIncoming};
use srtla_protocol::*;
use srtla_core::registration::{RegistrationEvent, SrtlaRegistrationManager};

/// Process one received uplink datagram.
///
/// Updates the connection's protocol state and accumulates the receive-side
/// effects (forward-to-client bytes, ACK/NAK/SRTLA-ACK sequence numbers, a
/// deferred immediate-REG1 send) into a [`SrtlaIncoming`]. The one piece of
/// inline I/O it keeps is the latency-critical ACK fast-path: forwarding an SRT
/// ACK straight to the downstream client rather than paying a channel hop.
///
/// This used to be an inherent `impl SrtlaConnection` method; it lives in the
/// shell now so the connection type owns no receive-side socket. It still takes
/// the downstream client socket (`local_listener`) and the instant-forward
/// channel by reference — both are shell-owned, not connection state.
#[allow(clippy::too_many_arguments)]
pub async fn process_uplink_packet(
    conn: &mut SrtlaConnection,
    conn_idx: usize,
    reg: &mut SrtlaRegistrationManager,
    local_listener: &UdpSocket,
    instant_forwarder: &tokio::sync::mpsc::UnboundedSender<(SocketAddr, SmallVec<u8, 64>)>,
    client_addr: Option<SocketAddr>,
    data: &[u8],
) -> Result<SrtlaIncoming> {
    let mut incoming = SrtlaIncoming {
        read_any: true,
        ..Default::default()
    };
    let now = srtla_core::utils::now_ms();
    let pt = get_packet_type(data);
    if let Some(pt) = pt {
        if let Some(event) = reg.process_registration_packet(conn_idx, data, now) {
            match event {
                RegistrationEvent::RegNgp => {
                    // Answering REG_NGP with an immediate REG1 is deferred to
                    // the shell: build the packet here (advancing reg state)
                    // and stash it as an effect so this stays free of uplink I/O.
                    if let Some(pkt) = reg.reg1_if_ngp_immediate(conn_idx, now) {
                        incoming.reg1_send = Some(pkt);
                    }
                }
                RegistrationEvent::Reg3 => {
                    // Clear any phantom in-flight packets and NAK state
                    // accumulated during pre-registration data forwarding
                    conn.clear_pre_registration_state(now);
                    conn.connected = true;
                    conn.last_received = Some(now);
                    if conn.reconnection.connection_established_ms == 0 {
                        conn.reconnection.connection_established_ms = now;
                    }
                    conn.reconnection.mark_success(&conn.label);
                }
                RegistrationEvent::RegErr => {
                    conn.connected = false;
                    conn.last_received = None;
                }
                RegistrationEvent::Reg2 => {}
            }
            return Ok(incoming);
        }

        conn.last_received = Some(now);

        if pt == SRT_TYPE_ACK {
            if let Some(ack) = parse_srt_ack(data) {
                incoming.ack_numbers.push(ack);
            }
            let ack_packet = SmallVec::from_slice_copy(data);
            // Try synchronous send first (avoids task context switch)
            // Only fall back to channel if socket would block
            if let Some(addr) = client_addr {
                match local_listener.try_send_to(&ack_packet, addr) {
                    Ok(_) => {} // Fast path: sent directly
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        // Slow path: socket busy, use channel
                        let _ = instant_forwarder.send((addr, ack_packet.clone()));
                    }
                    Err(_) => {} // Other errors: drop silently (same as before)
                }
            }
            incoming.forward_to_client.push(ack_packet);
        } else if pt == SRT_TYPE_NAK {
            let nak_list = parse_srt_nak(data);
            if !nak_list.is_empty() {
                debug!(
                    "📦 NAK received from {}: {} sequences",
                    conn.label,
                    nak_list.len()
                );
                for seq in nak_list {
                    incoming.nak_numbers.push(seq);
                }
            }
            incoming
                .forward_to_client
                .push(SmallVec::from_slice_copy(data));
        } else if pt == SRTLA_TYPE_ACK {
            let ack_list = parse_srtla_ack(data);
            if !ack_list.is_empty() {
                debug!(
                    "🎯 SRTLA ACK received from {}: {} sequences",
                    conn.label,
                    ack_list.len()
                );
                for seq in ack_list {
                    incoming.srtla_ack_numbers.push(seq);
                }
            }
        } else if pt == SRTLA_TYPE_KEEPALIVE {
            if conn
                .rtt
                .handle_keepalive_response(data, &conn.label, now)
                .is_some()
            {
                conn.record_rtt_probe();
                // Delivery proof for `stall_deselect`: a completed keepalive
                // round-trip proves this link's path is alive even while no
                // data ACKs are landing. Pairs with the earned-ACK site
                // (see `ack_nak.rs`); together they let a recovered link
                // un-gate itself without the scheduler probing blindly.
                conn.last_ack_or_rtt_sample_ms = now;
            }
        } else {
            incoming
                .forward_to_client
                .push(SmallVec::from_slice_copy(data));
        }
    }
    Ok(incoming)
}
