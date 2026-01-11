use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use smallvec::SmallVec;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio::task::JoinHandle;
use tokio::time::Duration;
use tracing::warn;

use crate::connection::SrtlaConnection;
use crate::connection::batch_recv::{BatchUdpSocket, RecvMmsgBuffer};

pub type ConnectionId = u64;

pub struct UplinkPacket {
    pub conn_id: ConnectionId,
    pub bytes: SmallVec<u8, 64>,
}

pub struct ReaderHandle {
    pub handle: JoinHandle<()>,
}

/// Spawn a reader task for a connection.
///
/// On Unix: Uses `recvmmsg` via `BatchUdpSocket` to batch receive up to 32 packets
/// per syscall, significantly reducing syscall overhead at high packet rates.
///
/// On non-Unix: Falls back to tokio's async recv_from (one packet per call).
pub fn spawn_reader(
    conn_id: ConnectionId,
    label: String,
    socket: Arc<BatchUdpSocket>,
    packet_tx: UnboundedSender<UplinkPacket>,
) -> ReaderHandle {
    let handle = tokio::spawn(async move {
        // Allocate batch receive buffer on heap (large structure ~50KB on Unix)
        let mut recv_buffer = RecvMmsgBuffer::new();

        loop {
            // Wait for and receive packets (batched on Unix, single on other platforms)
            match socket.recv_batch(&mut recv_buffer).await {
                Ok(count) if count > 0 => {
                    // Process all received packets
                    for (_addr, data) in recv_buffer.iter() {
                        if data.is_empty() {
                            continue;
                        }
                        let packet = SmallVec::from_slice_copy(data);
                        if packet_tx
                            .send(UplinkPacket {
                                conn_id,
                                bytes: packet,
                            })
                            .is_err()
                        {
                            return; // Channel closed, exit task
                        }
                    }
                }
                Ok(_) => {
                    // No packets received (shouldn't happen after await returns)
                }
                Err(err) => {
                    warn!("{}: uplink recv error: {}", label, err);
                    // Send empty packet to signal error to handler
                    if packet_tx
                        .send(UplinkPacket {
                            conn_id,
                            bytes: SmallVec::new(),
                        })
                        .is_err()
                    {
                        return;
                    }
                    // Pause before retrying to avoid CPU-intensive tight error loops.
                    // 100ms is long enough to prevent spinning but short enough to
                    // recover quickly when the error resolves.
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
        }
    });
    ReaderHandle { handle }
}

pub fn sync_readers(
    connections: &[SrtlaConnection],
    readers: &mut HashMap<ConnectionId, ReaderHandle>,
    packet_tx: &UnboundedSender<UplinkPacket>,
) {
    let mut active_ids = HashSet::with_capacity(connections.len());
    for conn in connections {
        active_ids.insert(conn.conn_id);
        readers.entry(conn.conn_id).or_insert_with(|| {
            spawn_reader(
                conn.conn_id,
                conn.label.clone(),
                conn.socket.clone(),
                packet_tx.clone(),
            )
        });
    }

    readers.retain(|conn_id, reader| {
        if active_ids.contains(conn_id) {
            true
        } else {
            reader.handle.abort();
            false
        }
    });
}

pub fn restart_reader_for(
    conn: &SrtlaConnection,
    readers: &mut HashMap<ConnectionId, ReaderHandle>,
    packet_tx: &UnboundedSender<UplinkPacket>,
) {
    if let Some(reader) = readers.remove(&conn.conn_id) {
        reader.handle.abort();
    }
    readers.insert(
        conn.conn_id,
        spawn_reader(
            conn.conn_id,
            conn.label.clone(),
            conn.socket.clone(),
            packet_tx.clone(),
        ),
    );
}

pub fn create_uplink_channel() -> (
    UnboundedSender<UplinkPacket>,
    UnboundedReceiver<UplinkPacket>,
) {
    unbounded_channel::<UplinkPacket>()
}
