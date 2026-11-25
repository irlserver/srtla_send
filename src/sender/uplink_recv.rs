use std::net::SocketAddr;

use anyhow::Result;
use smallvec::SmallVec;
use srtla_core::connection::{SrtlaConnection, SrtlaIncoming};
use srtla_core::registration::{RegistrationEvent, SrtlaRegistrationManager};
use srtla_protocol::*;
use tokio::net::UdpSocket;
use tracing::debug;

use super::client_dedup::ClientDedup;

/// Process one received uplink datagram.
///
/// Updates the connection's protocol state and accumulates the receive-side
/// effects (forward-to-client bytes, ACK/NAK/SRTLA-ACK sequence numbers, a
/// deferred immediate-REG1 send) into a [`SrtlaIncoming`]. The one piece of
/// inline I/O it keeps is the latency-critical ACK fast-path: forwarding an SRT
/// ACK straight to the downstream client rather than paying a channel hop.
/// `client_dedup` decides which ACK and NAK copies reach the client at all;
/// the ACK/NAK sequence numbers are reported for every copy regardless.
///
/// This used to be an inherent `impl SrtlaConnection` method; it lives in the
/// shell now so the connection type owns no receive-side socket. It still takes
/// the downstream client socket (`local_listener`) and the instant-forward
/// channel by reference — both are shell-owned, not connection state.
#[cfg(test)]
#[allow(clippy::too_many_arguments)]
pub async fn process_uplink_packet(
    conn: &mut SrtlaConnection,
    reg: &mut SrtlaRegistrationManager,
    local_listener: &UdpSocket,
    instant_forwarder: &tokio::sync::mpsc::UnboundedSender<(SocketAddr, SmallVec<u8, 64>)>,
    client_addr: Option<SocketAddr>,
    client_dedup: &mut ClientDedup,
    data: &[u8],
) -> Result<SrtlaIncoming> {
    process_uplink_packet_at(
        conn,
        reg,
        local_listener,
        instant_forwarder,
        client_addr,
        client_dedup,
        data,
        srtla_core::utils::now_ms(),
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn process_uplink_packet_at(
    conn: &mut SrtlaConnection,
    reg: &mut SrtlaRegistrationManager,
    local_listener: &UdpSocket,
    instant_forwarder: &tokio::sync::mpsc::UnboundedSender<(SocketAddr, SmallVec<u8, 64>)>,
    client_addr: Option<SocketAddr>,
    client_dedup: &mut ClientDedup,
    data: &[u8],
    now: u64,
) -> Result<SrtlaIncoming> {
    let mut incoming = SrtlaIncoming {
        read_any: true,
        ..Default::default()
    };
    let pt = ControlPacket::packet_type(data);
    if let Some(pt) = pt {
        if let Some(event) = reg.process_registration_packet(conn.conn_id, data, now) {
            match event {
                RegistrationEvent::RegNgp => {
                    // Answering REG_NGP with an immediate REG1 is deferred to
                    // the shell: build the packet here (advancing reg state)
                    // and stash it as an effect so this stays free of uplink I/O.
                    if let Some(pkt) = reg.reg1_if_ngp_immediate(conn.conn_id, now) {
                        incoming.reg1_send = Some(Box::new(pkt));
                    }
                }
                RegistrationEvent::Reg3 => {
                    conn.on_registered(now);
                }
                RegistrationEvent::RegErr => {
                    conn.on_registration_error();
                }
                RegistrationEvent::Reg2 => {}
            }
            return Ok(incoming);
        }

        conn.note_packet_received(now);

        if pt == SRT_TYPE_ACK {
            if let Some(ack) = ControlPacket::parse_srt_ack(data) {
                incoming.ack_numbers.push(ack);
            }
            // The ACK goes to the client here only, never through
            // `forward_to_client`: libsrt answers each ACK it reads with an
            // ACKACK on the bonded uplinks. Try a synchronous send first
            // (avoids a task context switch) and fall back to the channel only
            // if the socket would block.
            if let Some(addr) = client_addr
                && client_dedup.should_forward_ack(data, now)
            {
                match local_listener.try_send_to(data, addr) {
                    Ok(_) => {}
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        let _ = instant_forwarder.send((addr, SmallVec::from_slice_copy(data)));
                    }
                    Err(_) => {}
                }
            }
        } else if pt == SRT_TYPE_NAK {
            let nak_list = ControlPacket::parse_srt_nak(data);
            if !nak_list.is_empty() {
                debug!(
                    "📦 NAK received from {}: {} sequences",
                    conn.label,
                    nak_list.len()
                );
                incoming.nak_numbers = nak_list;
            }
            if client_addr.is_some() && client_dedup.should_forward_nak(data, now) {
                incoming
                    .forward_to_client
                    .push(SmallVec::from_slice_copy(data));
            }
        } else if pt == SRTLA_TYPE_ACK {
            let ack_list = SrtlaAck::parse(data);
            if !ack_list.is_empty() {
                debug!(
                    "🎯 SRTLA ACK received from {}: {} sequences",
                    conn.label,
                    ack_list.len()
                );
                incoming.srtla_ack_numbers = ack_list;
            }
        } else if pt == SRT_TYPE_HANDSHAKE {
            // The far end declares its TSBPD receive delay in the clear here,
            // and this is the only place we can see it: the scheduler otherwise
            // has to guess a delay budget from its own RTT samples. Only the
            // response is worth reading — an HSREQ crossing the other way is
            // the local caller's *proposal*, whereas the responder has already
            // resolved both sides to max(own, proposed) by the time it answers.
            if let Some(hs) = ControlPacket::parse_handshake_latency(data)
                && hs.is_response
                && let Some(rcv_ms) = hs.rcv_ms
            {
                debug!(
                    "{}: SRT peer declared a {}ms receive buffer",
                    conn.label, rcv_ms
                );
                incoming.negotiated_latency_ms = Some(rcv_ms);
            }
            // Sniffing only — the handshake still belongs to the client.
            incoming
                .forward_to_client
                .push(SmallVec::from_slice_copy(data));
        } else if pt == SRTLA_TYPE_KEEPALIVE {
            conn.handle_keepalive_echo(data, now);
        } else {
            incoming
                .forward_to_client
                .push(SmallVec::from_slice_copy(data));
        }
    }
    Ok(incoming)
}
