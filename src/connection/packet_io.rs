use std::net::SocketAddr;

use anyhow::Result;
use smallvec::SmallVec;
use tokio::net::UdpSocket;
use tokio::time::Instant;
use tracing::{debug, warn};

use super::SrtlaConnection;
use super::incoming::SrtlaIncoming;
use crate::protocol::*;
use crate::registration::{RegistrationEvent, SrtlaRegistrationManager};

impl SrtlaConnection {
    pub async fn drain_incoming(
        &mut self,
        conn_idx: usize,
        reg: &mut SrtlaRegistrationManager,
        local_listener: &UdpSocket,
        instant_forwarder: &tokio::sync::mpsc::UnboundedSender<(SocketAddr, SmallVec<u8, 64>)>,
        client_addr: Option<SocketAddr>,
    ) -> Result<SrtlaIncoming> {
        let mut buf = [0u8; MTU];
        let mut incoming = SrtlaIncoming::default();
        loop {
            match self.socket.try_recv(&mut buf) {
                Ok(n) => {
                    if n == 0 {
                        break;
                    }
                    self.process_packet_internal(
                        conn_idx,
                        reg,
                        local_listener,
                        instant_forwarder,
                        client_addr,
                        &buf[..n],
                        &mut incoming,
                    )
                    .await?;
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) => {
                    warn!("read uplink error: {}", e);
                    break;
                }
            }
        }
        Ok(incoming)
    }

    pub async fn process_packet(
        &mut self,
        conn_idx: usize,
        reg: &mut SrtlaRegistrationManager,
        local_listener: &UdpSocket,
        instant_forwarder: &tokio::sync::mpsc::UnboundedSender<(SocketAddr, SmallVec<u8, 64>)>,
        client_addr: Option<SocketAddr>,
        data: &[u8],
    ) -> Result<SrtlaIncoming> {
        let mut incoming = SrtlaIncoming::default();
        self.process_packet_internal(
            conn_idx,
            reg,
            local_listener,
            instant_forwarder,
            client_addr,
            data,
            &mut incoming,
        )
        .await?;
        Ok(incoming)
    }

    #[allow(clippy::too_many_arguments)]
    async fn process_packet_internal(
        &mut self,
        conn_idx: usize,
        reg: &mut SrtlaRegistrationManager,
        local_listener: &UdpSocket,
        instant_forwarder: &tokio::sync::mpsc::UnboundedSender<(SocketAddr, SmallVec<u8, 64>)>,
        client_addr: Option<SocketAddr>,
        data: &[u8],
        incoming: &mut SrtlaIncoming,
    ) -> Result<()> {
        incoming.read_any = true;
        let recv_time = Instant::now();
        let pt = get_packet_type(data);
        if let Some(pt) = pt {
            if let Some(event) = reg.process_registration_packet(conn_idx, data) {
                match event {
                    RegistrationEvent::RegNgp => {
                        reg.try_send_reg1_immediately(conn_idx, self).await;
                    }
                    RegistrationEvent::Reg3 => {
                        self.connected = true;
                        self.last_received = Some(recv_time);
                        if self.reconnection.connection_established_ms == 0 {
                            self.reconnection.connection_established_ms = crate::utils::now_ms();
                        }
                        self.reconnection.mark_success(&self.label);
                    }
                    RegistrationEvent::RegErr => {
                        self.connected = false;
                        self.last_received = None;
                    }
                    RegistrationEvent::Reg2 => {}
                }
                return Ok(());
            }

            self.last_received = Some(recv_time);

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
                        self.label,
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
                        self.label,
                        ack_list.len()
                    );
                    for seq in ack_list {
                        incoming.srtla_ack_numbers.push(seq);
                    }
                }
            } else if pt == SRTLA_TYPE_KEEPALIVE {
                self.rtt.handle_keepalive_response(data, &self.label);
            } else {
                incoming
                    .forward_to_client
                    .push(SmallVec::from_slice_copy(data));
            }
        }
        Ok(())
    }
}
