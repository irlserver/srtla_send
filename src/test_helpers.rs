#![cfg(any(test, feature = "test-internals"))]
// Allow unused helpers/re-exports: they're used by library tests but not binary
// tests (the re-exports below export nothing in the binary crate).
#![allow(dead_code, unused_imports)]

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;

use socket2::{Domain, Protocol, Socket, Type};
use srtla_core::connection::SrtlaConnection;
// The socket-free connection builders (create_test_connection[s]) and the tokio
// clock seam (advance_test_clock) live in srtla-core's test-internals surface,
// re-exported here so `crate::test_helpers::*` keeps resolving for shell tests.
pub use srtla_core::test_helpers::{
    advance_test_clock, create_test_connection, create_test_connections,
};

use crate::net::{BatchUdpSocket, SourceIpBinder};
use crate::sender::{ConnIo, ConnIoMap};

/// Build the I/O half for a test connection: a real localhost UDP socket
/// wrapped in a `BatchUdpSocket`. Used by tests that drive reader/reconnect I/O.
pub fn create_test_conn_io() -> ConnIo {
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP)).unwrap();
    socket
        .bind(&"127.0.0.1:0".parse::<SocketAddr>().unwrap().into())
        .unwrap();
    socket.set_nonblocking(true).unwrap();
    ConnIo {
        socket: Arc::new(BatchUdpSocket::new(socket).unwrap()),
        binder: Arc::new(SourceIpBinder),
        remote: SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8080),
    }
}

/// Build a `ConnIoMap` giving each connection its own localhost socket, keyed by
/// `conn_id` (matching the shell's real lifecycle).
pub fn create_test_conn_io_map(connections: &[SrtlaConnection]) -> ConnIoMap {
    connections
        .iter()
        .map(|c| (c.conn_id, create_test_conn_io()))
        .collect()
}
