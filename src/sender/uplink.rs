use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use smallvec::SmallVec;
use tokio::net::UdpSocket;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio::task::JoinHandle;
use tokio::time::Duration;
use tracing::warn;

use crate::connection::SrtlaConnection;
use crate::protocol::MTU;

pub type ConnectionId = u64;

pub struct UplinkPacket {
    pub conn_id: ConnectionId,
    pub bytes: SmallVec<u8, 64>,
}

pub struct ReaderHandle {
    pub handle: JoinHandle<()>,
}

pub fn spawn_reader(
    conn_id: ConnectionId,
    label: String,
    socket: Arc<UdpSocket>,
    packet_tx: UnboundedSender<UplinkPacket>,
) -> ReaderHandle {
    let handle = tokio::spawn(async move {
        let mut buf = vec![0u8; MTU];
        loop {
            match socket.recv_from(&mut buf).await {
                Ok((n, _)) if n > 0 => {
                    let packet = SmallVec::from_slice(&buf[..n]);
                    if packet_tx
                        .send(UplinkPacket {
                            conn_id,
                            bytes: packet,
                        })
                        .is_err()
                    {
                        break;
                    }
                }
                Ok(_) => {}
                Err(err) => {
                    warn!("{}: uplink recv error: {}", label, err);
                    if packet_tx
                        .send(UplinkPacket {
                            conn_id,
                            bytes: SmallVec::new(),
                        })
                        .is_err()
                    {
                        break;
                    }
                    // Allow brief pause before retrying to avoid tight error loops.
                    tokio::time::sleep(Duration::from_millis(10)).await;
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
