//! Batch receive implementation using `recvmmsg` on Linux platforms.
//!
//! This module provides efficient batch reception of UDP packets by using the
//! `recvmmsg` syscall on Linux, which can receive multiple datagrams in a
//! single kernel transition. On non-Linux platforms, it falls back to single-packet
//! receives.
//!
//! Based on the rustorrent implementation:
//! https://github.com/sebastiencs/rustorrent/blob/master/src/utp/udp_socket.rs

use crate::protocol::MTU;

/// Number of packets to receive in a single `recvmmsg` call.
/// 32 is a good balance between syscall reduction and memory usage.
#[cfg(target_os = "linux")]
pub const BATCH_RECV_SIZE: usize = 32;

// ============================================================================
// Linux implementation with recvmmsg
// ============================================================================

#[cfg(target_os = "linux")]
mod unix_impl {
    use std::io::ErrorKind;
    use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};
    use std::os::unix::io::{AsRawFd, RawFd};
    use std::task::{Context, Poll, ready};

    use socket2::Socket;
    use tokio::io::Interest;
    use tokio::io::unix::AsyncFd;

    use super::{BATCH_RECV_SIZE, MTU};

    const SOCKADDR_STORAGE_LENGTH: libc::socklen_t =
        std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;

    /// Async UDP socket with batch receive support via `recvmmsg`.
    ///
    /// This wraps a `socket2::Socket` in tokio's `AsyncFd` for proper async
    /// readability polling, then uses `recvmmsg` to receive multiple packets
    /// in a single syscall.
    pub struct BatchUdpSocket {
        inner: AsyncFd<Socket>,
    }

    impl BatchUdpSocket {
        /// Create a new BatchUdpSocket from a socket2::Socket.
        ///
        /// The socket must already be bound, connected, and set to non-blocking mode.
        pub fn new(socket: Socket) -> std::io::Result<Self> {
            Ok(Self {
                inner: AsyncFd::with_interest(socket, Interest::READABLE | Interest::WRITABLE)?,
            })
        }

        /// Get the raw file descriptor.
        pub fn as_raw_fd(&self) -> RawFd {
            self.inner.get_ref().as_raw_fd()
        }

        /// Poll for readability and receive multiple packets.
        pub fn poll_recv_batch(
            &self,
            cx: &mut Context<'_>,
            buffer: &mut RecvMmsgBuffer,
        ) -> Poll<std::io::Result<usize>> {
            loop {
                let mut guard = ready!(self.inner.poll_read_ready(cx))?;

                match buffer.recvmmsg(self.as_raw_fd()) {
                    Ok(count) => return Poll::Ready(Ok(count)),
                    Err(ref e) if e.kind() == ErrorKind::WouldBlock => {
                        guard.clear_ready();
                        continue;
                    }
                    Err(e) => return Poll::Ready(Err(e)),
                }
            }
        }

        /// Receive multiple packets asynchronously.
        ///
        /// Returns the number of packets received. Packets can be accessed
        /// via `buffer.iter()`.
        pub async fn recv_batch(&self, buffer: &mut RecvMmsgBuffer) -> std::io::Result<usize> {
            std::future::poll_fn(|cx| self.poll_recv_batch(cx, buffer)).await
        }

        /// Send data to the connected peer asynchronously.
        pub async fn send(&self, buf: &[u8]) -> std::io::Result<usize> {
            loop {
                let mut guard = self.inner.ready(Interest::WRITABLE).await?;

                match self.inner.get_ref().send(buf) {
                    Ok(n) => return Ok(n),
                    Err(ref e) if e.kind() == ErrorKind::WouldBlock => {
                        guard.clear_ready();
                        continue;
                    }
                    Err(e) => return Err(e),
                }
            }
        }

        /// Try to send data without blocking.
        ///
        /// Returns WouldBlock if the socket is not ready.
        #[allow(dead_code)]
        pub fn try_send(&self, buf: &[u8]) -> std::io::Result<usize> {
            self.inner.get_ref().send(buf)
        }

        /// Try to receive data without blocking.
        ///
        /// Returns WouldBlock if no data is available.
        #[allow(dead_code)]
        pub fn try_recv(&self, buf: &mut [u8]) -> std::io::Result<usize> {
            use std::mem::MaybeUninit;

            // Safety: We're using MaybeUninit slice for the socket2 API,
            // but the recv call will initialize the bytes it writes.
            let buf_uninit: &mut [MaybeUninit<u8>] =
                unsafe { &mut *(buf as *mut [u8] as *mut [MaybeUninit<u8>]) };
            self.inner.get_ref().recv(buf_uninit)
        }

        /// Get a reference to the underlying socket.
        #[allow(dead_code)]
        pub fn get_ref(&self) -> &Socket {
            self.inner.get_ref()
        }
    }

    impl AsRawFd for BatchUdpSocket {
        fn as_raw_fd(&self) -> RawFd {
            self.inner.get_ref().as_raw_fd()
        }
    }

    /// Buffer for batch receiving multiple UDP packets via `recvmmsg`.
    pub struct RecvMmsgBuffer {
        /// Storage for source addresses
        addr_storage: [libc::sockaddr_storage; BATCH_RECV_SIZE],
        /// IO vectors pointing to packet buffers
        iov: [libc::iovec; BATCH_RECV_SIZE],
        /// Message headers for recvmmsg
        mmsghdr: [libc::mmsghdr; BATCH_RECV_SIZE],
        /// Packet data buffers
        buffers: [[u8; MTU]; BATCH_RECV_SIZE],
        /// Number of packets received in last call
        nrecv: u32,
    }

    // Safety: All fields are either Copy types or raw pointers that point
    // to data within this struct. The struct is self-contained.
    unsafe impl Send for RecvMmsgBuffer {}

    impl RecvMmsgBuffer {
        /// Create a new batch receive buffer.
        ///
        /// This allocates the buffer on the heap due to its large size (~50KB).
        pub fn new() -> Box<Self> {
            // Safety: We're zeroing memory that will be properly initialized
            // before use. The iov and mmsghdr pointers are set up below.
            let mut ptr: Box<Self> = Box::new(unsafe { std::mem::zeroed() });

            let buffers = ptr.buffers.as_mut_ptr();

            // Set up IO vectors to point to packet buffers
            ptr.iov.iter_mut().enumerate().for_each(|(index, iov)| {
                let buffer = unsafe { &mut *buffers.add(index) };
                *iov = libc::iovec {
                    iov_base: buffer.as_mut_ptr() as *mut libc::c_void,
                    iov_len: buffer.len(),
                }
            });

            let addrs = ptr.addr_storage.as_mut_ptr();
            let iov = ptr.iov.as_mut_ptr();

            // Set up message headers
            ptr.mmsghdr.iter_mut().enumerate().for_each(|(index, h)| {
                *h = libc::mmsghdr {
                    msg_hdr: libc::msghdr {
                        msg_name: unsafe { addrs.add(index) as *mut libc::c_void },
                        msg_namelen: SOCKADDR_STORAGE_LENGTH,
                        msg_iov: unsafe { iov.add(index) },
                        msg_iovlen: 1,
                        msg_control: std::ptr::null_mut(),
                        msg_controllen: 0,
                        msg_flags: 0,
                    },
                    msg_len: 0,
                }
            });

            ptr.nrecv = 0;
            ptr
        }

        /// Reset the buffer for the next recvmmsg call.
        fn init(&mut self) {
            self.mmsghdr.iter_mut().for_each(|h| {
                h.msg_hdr.msg_namelen = SOCKADDR_STORAGE_LENGTH;
            });
        }

        /// Receive multiple packets using recvmmsg.
        ///
        /// Returns Ok(count) with the number of packets received, or Err if the syscall failed.
        /// WouldBlock errors indicate no data is available (non-blocking socket).
        pub fn recvmmsg(&mut self, fd: RawFd) -> std::io::Result<usize> {
            self.init();

            let result = unsafe {
                libc::recvmmsg(
                    fd,
                    self.mmsghdr.as_mut_ptr(),
                    self.mmsghdr.len() as u32,
                    libc::MSG_DONTWAIT, // Non-blocking
                    std::ptr::null_mut(),
                )
            };

            if result == -1 {
                self.nrecv = 0;
                return Err(std::io::Error::last_os_error());
            }

            self.nrecv = result as u32;
            Ok(result as usize)
        }

        /// Get an iterator over the received packets.
        pub fn iter(&self) -> RecvMmsgIter<'_> {
            RecvMmsgIter {
                buffer: self,
                current: 0,
            }
        }

        /// Get the number of packets received in the last call.
        #[cfg(test)]
        pub fn len(&self) -> usize {
            self.nrecv as usize
        }

        /// Check if no packets were received.
        #[cfg(test)]
        pub fn is_empty(&self) -> bool {
            self.nrecv == 0
        }
    }

    /// Iterator over received packets in a RecvMmsgBuffer.
    pub struct RecvMmsgIter<'a> {
        buffer: &'a RecvMmsgBuffer,
        current: u32,
    }

    impl<'a> Iterator for RecvMmsgIter<'a> {
        /// Returns (source_address, packet_data)
        type Item = (Option<SocketAddr>, &'a [u8]);

        fn next(&mut self) -> Option<Self::Item> {
            if self.current >= self.buffer.nrecv {
                return None;
            }

            let idx = self.current as usize;
            self.current += 1;

            let msg = &self.buffer.mmsghdr[idx];
            let storage = &self.buffer.addr_storage[idx];

            // Convert sockaddr_storage to SocketAddr
            let addr = sockaddr_storage_to_socket_addr(storage);

            let data = &self.buffer.buffers[idx][..msg.msg_len as usize];
            Some((addr, data))
        }
    }

    /// Convert a libc::sockaddr_storage to a std::net::SocketAddr
    fn sockaddr_storage_to_socket_addr(storage: &libc::sockaddr_storage) -> Option<SocketAddr> {
        // Safety: We're reading from a sockaddr_storage that was filled by recvmmsg
        unsafe {
            match storage.ss_family as libc::c_int {
                libc::AF_INET => {
                    let addr_in = storage as *const _ as *const libc::sockaddr_in;
                    let ip = Ipv4Addr::from(u32::from_be((*addr_in).sin_addr.s_addr));
                    let port = u16::from_be((*addr_in).sin_port);
                    Some(SocketAddr::V4(SocketAddrV4::new(ip, port)))
                }
                libc::AF_INET6 => {
                    let addr_in6 = storage as *const _ as *const libc::sockaddr_in6;
                    let ip = Ipv6Addr::from((*addr_in6).sin6_addr.s6_addr);
                    let port = u16::from_be((*addr_in6).sin6_port);
                    let flowinfo = (*addr_in6).sin6_flowinfo;
                    let scope_id = (*addr_in6).sin6_scope_id;
                    Some(SocketAddr::V6(SocketAddrV6::new(
                        ip, port, flowinfo, scope_id,
                    )))
                }
                _ => None,
            }
        }
    }
}

// ============================================================================
// Non-Linux fallback implementation
// ============================================================================

#[cfg(not(target_os = "linux"))]
mod fallback_impl {
    use std::net::SocketAddr;

    use socket2::Socket;
    use tokio::net::UdpSocket;

    use super::MTU;

    /// Fallback async UDP socket for non-Linux platforms.
    ///
    /// Uses tokio's UdpSocket directly since recvmmsg is not available.
    pub struct BatchUdpSocket {
        inner: UdpSocket,
    }

    impl BatchUdpSocket {
        /// Create a new BatchUdpSocket from a socket2::Socket.
        ///
        /// The socket must already be bound, connected, and set to non-blocking mode.
        pub fn new(socket: Socket) -> std::io::Result<Self> {
            // Convert socket2::Socket to std::net::UdpSocket
            let std_socket: std::net::UdpSocket = socket.into();
            Ok(Self {
                inner: UdpSocket::from_std(std_socket)?,
            })
        }

        /// Receive packets (single packet at a time on non-Unix).
        pub async fn recv_batch(&self, buffer: &mut RecvMmsgBuffer) -> std::io::Result<usize> {
            match self.inner.recv_from(&mut buffer.buffer).await {
                Ok((n, addr)) => {
                    buffer.len = n;
                    buffer.addr = Some(addr);
                    buffer.has_packet = true;
                    Ok(1)
                }
                Err(e) => {
                    buffer.has_packet = false;
                    Err(e)
                }
            }
        }

        /// Send data to the connected peer.
        pub async fn send(&self, buf: &[u8]) -> std::io::Result<usize> {
            self.inner.send(buf).await
        }

        /// Try to send data without blocking.
        #[allow(dead_code)]
        pub fn try_send(&self, buf: &[u8]) -> std::io::Result<usize> {
            self.inner.try_send(buf)
        }

        /// Try to receive data without blocking.
        #[allow(dead_code)]
        pub fn try_recv(&self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.inner.try_recv(buf)
        }

        /// Get a reference to the underlying socket.
        #[allow(dead_code)]
        pub fn get_ref(&self) -> &UdpSocket {
            &self.inner
        }
    }

    /// Fallback buffer for non-Unix platforms.
    pub struct RecvMmsgBuffer {
        /// Single packet buffer
        pub(super) buffer: [u8; MTU],
        /// Length of received packet
        pub(super) len: usize,
        /// Source address
        pub(super) addr: Option<SocketAddr>,
        /// Whether a packet was received
        pub(super) has_packet: bool,
    }

    impl RecvMmsgBuffer {
        pub fn new() -> Box<Self> {
            Box::new(Self {
                buffer: [0u8; MTU],
                len: 0,
                addr: None,
                has_packet: false,
            })
        }

        pub fn iter(&self) -> RecvMmsgIter<'_> {
            RecvMmsgIter {
                buffer: self,
                yielded: false,
            }
        }

        pub fn len(&self) -> usize {
            if self.has_packet { 1 } else { 0 }
        }

        pub fn is_empty(&self) -> bool {
            !self.has_packet
        }
    }

    impl Default for RecvMmsgBuffer {
        fn default() -> Self {
            Self {
                buffer: [0u8; MTU],
                len: 0,
                addr: None,
                has_packet: false,
            }
        }
    }

    pub struct RecvMmsgIter<'a> {
        buffer: &'a RecvMmsgBuffer,
        yielded: bool,
    }

    impl<'a> Iterator for RecvMmsgIter<'a> {
        type Item = (Option<SocketAddr>, &'a [u8]);

        fn next(&mut self) -> Option<Self::Item> {
            if self.yielded || !self.buffer.has_packet {
                return None;
            }
            self.yielded = true;
            Some((self.buffer.addr, &self.buffer.buffer[..self.buffer.len]))
        }
    }
}

// Re-export the appropriate implementation
#[cfg(not(target_os = "linux"))]
pub use fallback_impl::{BatchUdpSocket, RecvMmsgBuffer};
#[cfg(target_os = "linux")]
pub use unix_impl::{BatchUdpSocket, RecvMmsgBuffer};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_recv_buffer_creation() {
        let buffer = RecvMmsgBuffer::new();
        assert!(buffer.is_empty());
        assert_eq!(buffer.len(), 0);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_buffer_size() {
        // Verify buffer size is reasonable
        let size = std::mem::size_of::<RecvMmsgBuffer>();
        // Should be roughly 32 * 1500 + overhead ≈ 48KB + headers
        assert!(size > 32 * 1400);
        assert!(size < 100_000);
    }
}
