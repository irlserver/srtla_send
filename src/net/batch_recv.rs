//! Batch receive implementation using `recvmmsg` on Linux platforms.
//!
//! This module provides efficient batch reception of UDP packets by using the
//! `recvmmsg` syscall on Linux, which can receive multiple datagrams in a
//! single kernel transition. On non-Linux platforms, it falls back to single-packet
//! receives.
//!
//! Based on the rustorrent implementation:
//! https://github.com/sebastiencs/rustorrent/blob/master/src/utp/udp_socket.rs

// `BATCH_SEND_SIZE` is owned by the core (`connection::batch_send`) so the pure
// drain and this socket layer agree without core depending on `net`.
#[cfg(target_os = "linux")]
use srtla_core::connection::BATCH_SEND_SIZE;
use srtla_protocol::MTU;

/// Source of [`BatchUdpSocket::generation`] values. Starts at 1 so 0 never names
/// a real socket.
static NEXT_SOCKET_GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

fn next_socket_generation() -> u64 {
    NEXT_SOCKET_GENERATION.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

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

    use socket2::{SockAddr, SockRef, Socket};
    use tokio::io::Interest;
    use tokio::net::UdpSocket;

    use super::{BATCH_RECV_SIZE, BATCH_SEND_SIZE, MTU};

    const SOCKADDR_STORAGE_LENGTH: libc::socklen_t =
        std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;

    /// What the read loop should do after `recvmmsg` returns an error.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum RecvAction {
        /// EINTR: interrupted by a signal — re-issue the syscall.
        Retry,
        /// EAGAIN/EWOULDBLOCK: no datagram ready — wait for readiness.
        WouldBlock,
        /// Any other errno — propagate to the caller.
        Hard,
    }

    /// Classify a `recvmmsg` error. Pure so it is unit-testable with no syscall.
    fn recv_retry_action(err: &std::io::Error) -> RecvAction {
        match err.kind() {
            ErrorKind::Interrupted => RecvAction::Retry,
            ErrorKind::WouldBlock => RecvAction::WouldBlock,
            _ => RecvAction::Hard,
        }
    }

    /// Async UDP socket with batch receive support via `recvmmsg`.
    ///
    /// Readiness comes from tokio's `UdpSocket`; the batch syscalls
    /// (`recvmmsg`, `sendmmsg`) run inside its `try_io`, so a `WouldBlock`
    /// clears readiness exactly as tokio's own I/O methods do.
    ///
    /// The socket is deliberately **not** connected: `peer` carries the receiver
    /// address explicitly on every send, and the receive path accepts datagrams
    /// from any source. See [`BatchUdpSocket::new`].
    pub struct BatchUdpSocket {
        inner: UdpSocket,
        peer: SockAddr,
        generation: u64,
    }

    impl BatchUdpSocket {
        /// Create a new BatchUdpSocket from a socket2::Socket, sending to `peer`.
        ///
        /// The socket must already be bound and set to non-blocking mode, and
        /// must **not** be connected. A connected UDP socket makes the kernel
        /// drop any datagram whose source differs from the connect address,
        /// which breaks two real deployments: a multi-homed receiver that replies
        /// from a different local address than the one we sent to, and a NAT or
        /// load balancer that rewrites the reply source. The reference C sender
        /// binds its source address and then uses `sendto`/`recvfrom` for the
        /// same reason, so accepting any source is also the interoperable
        /// behavior.
        pub fn new(socket: Socket, peer: SocketAddr) -> std::io::Result<Self> {
            Ok(Self {
                inner: UdpSocket::from_std(socket.into())?,
                peer: peer.into(),
                generation: super::next_socket_generation(),
            })
        }

        /// Process-unique id of this socket. A rebuilt uplink gets a new
        /// socket and so a new generation, which lets the receive path drop
        /// datagrams its old socket queued before the rebuild.
        pub fn generation(&self) -> u64 {
            self.generation
        }

        /// Get the raw file descriptor.
        pub fn as_raw_fd(&self) -> RawFd {
            self.inner.as_raw_fd()
        }

        /// Poll for readability and receive multiple packets (with source addresses).
        pub fn poll_recv_batch(
            &self,
            cx: &mut Context<'_>,
            buffer: &mut RecvMmsgBuffer,
        ) -> Poll<std::io::Result<usize>> {
            poll_udp_recv_batch(&self.inner, cx, buffer, true)
        }

        /// Poll for readability and receive multiple packets without copying
        /// source addresses out of the kernel.
        pub fn poll_recv_batch_no_addr(
            &self,
            cx: &mut Context<'_>,
            buffer: &mut RecvMmsgBuffer,
        ) -> Poll<std::io::Result<usize>> {
            poll_udp_recv_batch(&self.inner, cx, buffer, false)
        }

        /// Receive multiple packets asynchronously.
        ///
        /// Returns the number of packets received. Packets can be accessed
        /// via `buffer.iter()`.
        pub async fn recv_batch(&self, buffer: &mut RecvMmsgBuffer) -> std::io::Result<usize> {
            std::future::poll_fn(|cx| self.poll_recv_batch(cx, buffer)).await
        }

        /// Receive multiple packets asynchronously without decoding source addresses.
        ///
        /// Returns the number of packets received. Packets can be accessed via
        /// `buffer.iter_payloads()`.
        pub async fn recv_batch_no_addr(
            &self,
            buffer: &mut RecvMmsgBuffer,
        ) -> std::io::Result<usize> {
            std::future::poll_fn(|cx| self.poll_recv_batch_no_addr(cx, buffer)).await
        }

        /// Send data to the peer asynchronously.
        pub async fn send(&self, buf: &[u8]) -> std::io::Result<usize> {
            self.inner
                .async_io(Interest::WRITABLE, || {
                    SockRef::from(&self.inner).send_to(buf, &self.peer)
                })
                .await
        }

        /// Try to send data without blocking.
        ///
        /// Returns WouldBlock if the socket is not ready.
        #[allow(dead_code)]
        pub fn try_send(&self, buf: &[u8]) -> std::io::Result<usize> {
            self.inner.try_io(Interest::WRITABLE, || {
                SockRef::from(&self.inner).send_to(buf, &self.peer)
            })
        }

        /// Send several datagrams to the peer in one `sendmmsg` syscall.
        ///
        /// Returns the number of datagrams the kernel accepted, which may be
        /// fewer than requested: `sendmmsg` reports a short send rather than
        /// blocking once the socket buffer fills. The caller must resend the
        /// remainder (see `BatchSender::flush`).
        pub async fn send_batch(&self, bufs: &[&[u8]]) -> std::io::Result<usize> {
            if bufs.is_empty() {
                return Ok(0);
            }
            loop {
                // `async_io` waits for writability and retries on `WouldBlock`.
                match self
                    .inner
                    .async_io(Interest::WRITABLE, || self.try_send_batch(bufs))
                    .await
                {
                    // A signal can interrupt the syscall before any datagram is
                    // queued; that is not a send failure, so retry rather than
                    // tearing the link down.
                    Err(ref e) if e.kind() == ErrorKind::Interrupted => continue,
                    result => return result,
                }
            }
        }

        /// Non-blocking `sendmmsg`. Sends at most [`BATCH_SEND_SIZE`] datagrams.
        pub fn try_send_batch(&self, bufs: &[&[u8]]) -> std::io::Result<usize> {
            let n = bufs.len().min(BATCH_SEND_SIZE);
            if n == 0 {
                return Ok(0);
            }

            // Only initialize the `0..n` slots we actually send.
            let mut iov: [std::mem::MaybeUninit<libc::iovec>; BATCH_SEND_SIZE] =
                [const { std::mem::MaybeUninit::uninit() }; BATCH_SEND_SIZE];
            let mut msgs: [std::mem::MaybeUninit<libc::mmsghdr>; BATCH_SEND_SIZE] =
                [const { std::mem::MaybeUninit::uninit() }; BATCH_SEND_SIZE];

            for i in 0..n {
                let iov_ptr = iov[i].write(libc::iovec {
                    // sendmmsg only reads through this pointer; the cast to
                    // *mut is required by the C signature, not by us.
                    iov_base: bufs[i].as_ptr() as *mut libc::c_void,
                    iov_len: bufs[i].len(),
                });
                // SAFETY: `mmsghdr` is a plain C struct whose all-zero bit
                // pattern is valid; every field we rely on is populated below.
                let mut msg: libc::mmsghdr = unsafe { std::mem::zeroed() };
                // The socket is unconnected, so every datagram carries its
                // destination. `sendmmsg` only reads through msg_name; the cast
                // to *mut is required by the C signature.
                msg.msg_hdr.msg_name = self.peer.as_ptr() as *mut libc::c_void;
                msg.msg_hdr.msg_namelen = self.peer.len();
                msg.msg_hdr.msg_iov = iov_ptr;
                msg.msg_hdr.msg_iovlen = 1;
                msgs[i].write(msg);
            }

            // SAFETY: `msgs[..n]` is initialised above and each `msg_iov` points
            // at the matching live entry of `iov`, which outlives the call. The
            // buffers in `bufs` are borrowed for the duration of the call.
            let ret = unsafe {
                libc::sendmmsg(
                    self.as_raw_fd(),
                    msgs.as_mut_ptr() as *mut libc::mmsghdr,
                    n as libc::c_uint,
                    0, // no flags: match the semantics of the old per-packet send()
                )
            };

            if ret < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(ret as usize)
        }

        /// Try to receive data without blocking.
        ///
        /// Returns WouldBlock if no data is available.
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

    impl AsRawFd for BatchUdpSocket {
        fn as_raw_fd(&self) -> RawFd {
            self.inner.as_raw_fd()
        }
    }

    fn poll_udp_recv_batch(
        socket: &UdpSocket,
        cx: &mut Context<'_>,
        buffer: &mut RecvMmsgBuffer,
        want_addr: bool,
    ) -> Poll<std::io::Result<usize>> {
        loop {
            ready!(socket.poll_recv_ready(cx))?;

            let fd = socket.as_raw_fd();
            match socket.try_io(Interest::READABLE, || {
                if want_addr {
                    buffer.recvmmsg(fd)
                } else {
                    buffer.recvmmsg_no_addr(fd)
                }
            }) {
                Ok(count) => return Poll::Ready(Ok(count)),
                Err(e) => match recv_retry_action(&e) {
                    // EINTR: re-issue without dropping readiness (the fd is
                    // still ready, so the next poll returns immediately). A
                    // signal — e.g. our own SIGHUP — must not kill the reader.
                    RecvAction::Retry => continue,
                    // `try_io` already cleared readiness, so the next poll
                    // parks until the reactor reports the fd readable.
                    RecvAction::WouldBlock => continue,
                    RecvAction::Hard => return Poll::Ready(Err(e)),
                },
            }
        }
    }

    /// Receive a batch of datagrams (with source addresses) from any tokio
    /// `UdpSocket` using a single `recvmmsg` syscall.
    pub async fn recv_udp_batch(
        socket: &UdpSocket,
        buffer: &mut RecvMmsgBuffer,
    ) -> std::io::Result<usize> {
        std::future::poll_fn(|cx| poll_udp_recv_batch(socket, cx, buffer, true)).await
    }

    /// Buffer for batch receiving multiple UDP packets via `recvmmsg`.
    ///
    /// The value may be moved freely: `init_for` compares `self`'s address
    /// against `initialized_addr` and rebuilds the self-referential pointers
    /// whenever the value has moved or switched address mode.
    pub struct RecvMmsgBuffer {
        /// Storage for source addresses
        addr_storage: [libc::sockaddr_storage; BATCH_RECV_SIZE],
        /// IO vectors pointing to packet buffers
        iov: [libc::iovec; BATCH_RECV_SIZE],
        /// Message headers for recvmmsg
        mmsghdr: [libc::mmsghdr; BATCH_RECV_SIZE],
        /// Packet data buffers
        buffers: [[u8; MTU]; BATCH_RECV_SIZE],
        /// Address of `self` when pointers were last initialised (0 = uninit).
        initialized_addr: usize,
        /// Number of packets received in last call
        nrecv: u32,
        /// Number of leading `mmsghdr` entries whose `msg_namelen` needs reset.
        dirty_count: u32,
        /// Whether pointers were last initialised with `msg_name` populated.
        initialized_with_addr: bool,
    }

    // Safety: the struct holds raw pointers into its own fields and nothing
    // else. `init_for` detects any address change and rewrites them before the
    // syscall, so moving the value to another thread between calls is sound.
    unsafe impl Send for RecvMmsgBuffer {}

    impl RecvMmsgBuffer {
        /// Create a new batch receive buffer directly on the heap (~56KB)
        /// without staging a zeroed copy on the stack.
        pub fn new() -> Box<Self> {
            let layout = std::alloc::Layout::new::<Self>();
            // Safety: `RecvMmsgBuffer` is non-zero-sized and every field is
            // valid when zeroed (integers, byte arrays, bool, null pointers).
            // `init_for` points the headers at this value before each syscall.
            let ptr = unsafe { std::alloc::alloc_zeroed(layout) as *mut Self };
            if ptr.is_null() {
                std::alloc::handle_alloc_error(layout);
            }
            unsafe { Box::from_raw(ptr) }
        }

        /// Point every header at this buffer's current address (with source
        /// addresses enabled) and reset `msg_namelen`.
        #[cfg(test)]
        fn init(&mut self) {
            self.init_for(true);
        }

        /// Ensure every header points at this buffer's current address in the
        /// requested address-capture mode, doing only minimal `msg_namelen`
        /// resets on steady-state calls where `self` did not move.
        fn init_for(&mut self, want_addr: bool) {
            let self_addr = self as *const Self as usize;
            if self.initialized_addr != self_addr || self.initialized_with_addr != want_addr {
                let buffers = self.buffers.as_mut_ptr();

                self.iov.iter_mut().enumerate().for_each(|(index, iov)| {
                    let buffer = unsafe { &mut *buffers.add(index) };
                    iov.iov_base = buffer.as_mut_ptr() as *mut libc::c_void;
                    iov.iov_len = buffer.len();
                });

                let addrs = self.addr_storage.as_mut_ptr();
                let iov = self.iov.as_mut_ptr();

                self.mmsghdr.iter_mut().enumerate().for_each(|(index, h)| {
                    if want_addr {
                        h.msg_hdr.msg_name = unsafe { addrs.add(index) as *mut libc::c_void };
                        h.msg_hdr.msg_namelen = SOCKADDR_STORAGE_LENGTH;
                    } else {
                        h.msg_hdr.msg_name = std::ptr::null_mut();
                        h.msg_hdr.msg_namelen = 0;
                    }
                    h.msg_hdr.msg_iov = unsafe { iov.add(index) };
                    h.msg_hdr.msg_iovlen = 1;
                });

                self.initialized_addr = self_addr;
                self.initialized_with_addr = want_addr;
                self.dirty_count = 0;
            } else if want_addr && self.dirty_count > 0 {
                let reset_n = (self.dirty_count as usize).min(BATCH_RECV_SIZE);
                for h in &mut self.mmsghdr[..reset_n] {
                    h.msg_hdr.msg_namelen = SOCKADDR_STORAGE_LENGTH;
                }
                self.dirty_count = 0;
            }
        }

        /// Receive multiple packets using recvmmsg (capturing source addresses).
        ///
        /// Returns Ok(count) with the number of packets received, or Err if the syscall failed.
        /// WouldBlock errors indicate no data is available (non-blocking socket).
        pub fn recvmmsg(&mut self, fd: RawFd) -> std::io::Result<usize> {
            self.recvmmsg_inner(fd, true)
        }

        /// Receive multiple packets using recvmmsg without copying source
        /// addresses out of the kernel.
        pub fn recvmmsg_no_addr(&mut self, fd: RawFd) -> std::io::Result<usize> {
            self.recvmmsg_inner(fd, false)
        }

        fn recvmmsg_inner(&mut self, fd: RawFd, want_addr: bool) -> std::io::Result<usize> {
            self.init_for(want_addr);

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

            let count = result as u32;
            self.nrecv = count;
            if want_addr {
                self.dirty_count = count;
            }
            Ok(result as usize)
        }

        /// Get an iterator over the received packets with source addresses.
        pub fn iter(&self) -> RecvMmsgIter<'_> {
            RecvMmsgIter {
                buffer: self,
                current: 0,
            }
        }

        /// Get an iterator over the received packet payloads without decoding
        /// source addresses.
        pub fn iter_payloads(&self) -> RecvMmsgPayloadIter<'_> {
            RecvMmsgPayloadIter {
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

        /// Test seam: forge `nrecv` "received" packets and set message `idx`'s
        /// reported `msg_len`, so a test can feed an out-of-range length without a
        /// live socket and prove the iterator clamps the exposed slice to MTU.
        #[cfg(test)]
        pub fn test_forge_packet(&mut self, idx: usize, msg_len: u32, nrecv: u32) {
            self.mmsghdr[idx].msg_hdr.msg_namelen = SOCKADDR_STORAGE_LENGTH;
            self.mmsghdr[idx].msg_len = msg_len;
            self.nrecv = nrecv;
        }

        /// Test seam: whether every pointer `init` writes targets this value's
        /// own fields at its current address. Compares addresses only and
        /// dereferences nothing, so miri accepts it on stale pointers too.
        #[cfg(test)]
        pub fn test_pointers_target_self(&self) -> bool {
            (0..BATCH_RECV_SIZE).all(|i| {
                let hdr = &self.mmsghdr[i].msg_hdr;
                self.iov[i].iov_base as *const u8 == self.buffers[i].as_ptr()
                    && self.iov[i].iov_len == MTU
                    && std::ptr::eq(hdr.msg_iov as *const libc::iovec, &self.iov[i])
                    && hdr.msg_iovlen == 1
                    && std::ptr::eq(
                        hdr.msg_name as *const libc::sockaddr_storage,
                        &self.addr_storage[i],
                    )
            })
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

            // The per-message buffer is exactly MTU bytes. No MSG_TRUNC is
            // requested so msg_len is capped at MTU in practice, but clamp
            // defensively so a mis-reported length can never index past it.
            let len = (msg.msg_len as usize).min(MTU);
            let data = &self.buffer.buffers[idx][..len];
            Some((addr, data))
        }
    }

    /// Iterator over received packet payloads in a RecvMmsgBuffer (no sockaddr decode).
    pub struct RecvMmsgPayloadIter<'a> {
        buffer: &'a RecvMmsgBuffer,
        current: u32,
    }

    impl<'a> Iterator for RecvMmsgPayloadIter<'a> {
        type Item = &'a [u8];

        fn next(&mut self) -> Option<Self::Item> {
            if self.current >= self.buffer.nrecv {
                return None;
            }

            let idx = self.current as usize;
            self.current += 1;

            let len = (self.buffer.mmsghdr[idx].msg_len as usize).min(MTU);
            Some(&self.buffer.buffers[idx][..len])
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

    #[cfg(test)]
    mod tests {
        use std::io::{Error, ErrorKind};
        use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};
        use std::time::Duration;

        use srtla_protocol::MTU;

        use super::{
            BatchUdpSocket, RecvAction, RecvMmsgBuffer, recv_retry_action,
            sockaddr_storage_to_socket_addr,
        };

        fn loopback_socket() -> socket2::Socket {
            let sock = socket2::Socket::new(
                socket2::Domain::IPV4,
                socket2::Type::DGRAM,
                Some(socket2::Protocol::UDP),
            )
            .unwrap();
            sock.set_nonblocking(true).unwrap();
            sock.bind(&SocketAddr::from((Ipv4Addr::LOCALHOST, 0)).into())
                .unwrap();
            sock
        }

        fn local_addr(sock: &socket2::Socket) -> SocketAddr {
            sock.local_addr().unwrap().as_socket().unwrap()
        }

        // The only test that drives the reactor readiness path end to end:
        // `sendmmsg` and `send` out of one socket, `recvmmsg` into the other.
        // Real syscalls, so not under miri.
        #[tokio::test]
        #[cfg_attr(miri, ignore)]
        async fn batch_send_and_recv_round_trip_over_loopback() {
            let a = loopback_socket();
            let b = loopback_socket();
            let (a_addr, b_addr) = (local_addr(&a), local_addr(&b));
            let a = BatchUdpSocket::new(a, b_addr).unwrap();
            let b = BatchUdpSocket::new(b, a_addr).unwrap();

            let payloads: [&[u8]; 3] = [b"one", b"two", b"three"];
            assert_eq!(a.send_batch(&payloads).await.unwrap(), 3);
            assert_eq!(a.send(b"four").await.unwrap(), 4);

            let mut buffer = RecvMmsgBuffer::new();
            let mut received = Vec::new();
            while received.len() < 4 {
                let n = tokio::time::timeout(
                    std::time::Duration::from_secs(5),
                    b.recv_batch(&mut buffer),
                )
                .await
                .expect("datagrams should arrive on loopback")
                .unwrap();
                assert!(n > 0);
                for (from, data) in buffer.iter() {
                    assert_eq!(from, Some(a_addr));
                    received.push(data.to_vec());
                }
            }
            assert_eq!(received, [&b"one"[..], b"two", b"three", b"four"]);
        }

        // Documents for plan 015 that loopback never applies send backpressure,
        // so the head-of-line case cannot be reproduced here.
        #[tokio::test]
        #[cfg_attr(miri, ignore)]
        async fn send_batch_does_not_wait_on_loopback_with_a_tiny_send_buffer() {
            let a = loopback_socket();
            let b = loopback_socket();
            a.set_send_buffer_size(1).unwrap();
            let a = BatchUdpSocket::new(a, local_addr(&b)).unwrap();

            let payload = [0u8; 1316];
            let batch = [&payload[..]; 32];
            let mut timeouts = 0;
            for _ in 0..125 {
                match tokio::time::timeout(Duration::from_millis(100), a.send_batch(&batch)).await {
                    Ok(result) => {
                        result.expect("send_batch failed on loopback");
                    }
                    Err(_) => timeouts += 1,
                }
            }
            assert_eq!(
                timeouts, 0,
                "send_batch waited past 100ms {timeouts} times on loopback"
            );
        }

        // Exercises the unsafe sockaddr_storage → SocketAddr pointer casts with
        // real AF_INET/AF_INET6 payloads (the iterator test only ever feeds
        // zeroed storage, i.e. the None branch). Runs under miri in CI to vet
        // the casts and the big-endian field decodes.
        #[test]
        fn sockaddr_storage_roundtrip() {
            let mut storage: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
            let v4 = libc::sockaddr_in {
                sin_family: libc::AF_INET as libc::sa_family_t,
                sin_port: 8000u16.to_be(),
                sin_addr: libc::in_addr {
                    s_addr: u32::from(Ipv4Addr::new(192, 168, 1, 2)).to_be(),
                },
                sin_zero: [0; 8],
            };
            unsafe { std::ptr::write(&mut storage as *mut _ as *mut libc::sockaddr_in, v4) };
            assert_eq!(
                sockaddr_storage_to_socket_addr(&storage),
                Some(SocketAddr::V4(SocketAddrV4::new(
                    Ipv4Addr::new(192, 168, 1, 2),
                    8000
                ))),
            );

            let mut storage: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
            let v6 = libc::sockaddr_in6 {
                sin6_family: libc::AF_INET6 as libc::sa_family_t,
                sin6_port: 9000u16.to_be(),
                sin6_flowinfo: 7,
                sin6_addr: libc::in6_addr {
                    s6_addr: Ipv6Addr::LOCALHOST.octets(),
                },
                sin6_scope_id: 3,
            };
            unsafe { std::ptr::write(&mut storage as *mut _ as *mut libc::sockaddr_in6, v6) };
            assert_eq!(
                sockaddr_storage_to_socket_addr(&storage),
                Some(SocketAddr::V6(SocketAddrV6::new(
                    Ipv6Addr::LOCALHOST,
                    9000,
                    7,
                    3
                ))),
            );

            let mut storage: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
            storage.ss_family = libc::AF_UNIX as libc::sa_family_t;
            assert_eq!(sockaddr_storage_to_socket_addr(&storage), None);
        }

        #[test]
        fn iter_clamps_oversized_msg_len_to_mtu() {
            let mut buffer = RecvMmsgBuffer::new();
            buffer.test_forge_packet(0, (MTU as u32) * 4, 1);

            let mut iter = buffer.iter();
            let (_addr, data) = iter.next().expect("one forged packet");
            assert_eq!(data.len(), MTU, "oversized msg_len must clamp to MTU");
            assert!(iter.next().is_none(), "only one packet was forged");
        }

        #[test]
        fn pointers_follow_the_buffer_after_a_move_out_of_the_box() {
            let mut boxed = RecvMmsgBuffer::new();
            boxed.init();
            assert!(boxed.test_pointers_target_self());

            let mut moved: RecvMmsgBuffer = *boxed;
            assert!(
                !moved.test_pointers_target_self(),
                "pointers written before the move must be stale",
            );
            moved.init();
            assert!(moved.test_pointers_target_self());
        }

        #[test]
        fn pointers_follow_both_buffers_after_a_swap() {
            let mut a = RecvMmsgBuffer::new();
            let mut b = RecvMmsgBuffer::new();
            a.init();
            b.init();

            std::mem::swap(&mut *a, &mut *b);
            assert!(!a.test_pointers_target_self());
            assert!(!b.test_pointers_target_self());

            a.init();
            b.init();
            assert!(a.test_pointers_target_self());
            assert!(b.test_pointers_target_self());
        }

        #[test]
        fn forged_packet_reads_correctly_after_a_move() {
            let mut moved: RecvMmsgBuffer = *RecvMmsgBuffer::new();
            moved.init();
            moved.buffers[0][..4].copy_from_slice(&[1, 2, 3, 4]);
            moved.test_forge_packet(0, 4, 1);

            assert_eq!(
                moved.iter().next().map(|(_, data)| data.to_vec()),
                Some(vec![1, 2, 3, 4]),
            );
        }

        #[test]
        fn recv_retry_action_classifies_errors() {
            assert_eq!(
                recv_retry_action(&Error::from(ErrorKind::Interrupted)),
                RecvAction::Retry,
            );
            assert_eq!(
                recv_retry_action(&Error::from(ErrorKind::WouldBlock)),
                RecvAction::WouldBlock,
            );
            assert_eq!(
                recv_retry_action(&Error::from_raw_os_error(libc::ECONNREFUSED)),
                RecvAction::Hard,
            );
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
    ///
    /// As on Linux, the socket is left unconnected and `peer` supplies the
    /// destination on every send; see the Linux [`BatchUdpSocket::new`] for why.
    pub struct BatchUdpSocket {
        inner: UdpSocket,
        peer: SocketAddr,
        generation: u64,
    }

    impl BatchUdpSocket {
        /// Create a new BatchUdpSocket from a socket2::Socket, sending to `peer`.
        ///
        /// The socket must already be bound and set to non-blocking mode, and
        /// must not be connected.
        pub fn new(socket: Socket, peer: SocketAddr) -> std::io::Result<Self> {
            // Convert socket2::Socket to std::net::UdpSocket
            let std_socket: std::net::UdpSocket = socket.into();
            Ok(Self {
                inner: UdpSocket::from_std(std_socket)?,
                peer,
                generation: super::next_socket_generation(),
            })
        }

        /// Process-unique id of this socket; see the Linux variant.
        pub fn generation(&self) -> u64 {
            self.generation
        }

        /// Receive packets (single packet at a time on non-Linux).
        pub async fn recv_batch(&self, buffer: &mut RecvMmsgBuffer) -> std::io::Result<usize> {
            recv_udp_batch(&self.inner, buffer).await
        }

        /// Receive packets without decoding source addresses (same as
        /// `recv_batch` on non-Linux).
        pub async fn recv_batch_no_addr(
            &self,
            buffer: &mut RecvMmsgBuffer,
        ) -> std::io::Result<usize> {
            recv_udp_batch(&self.inner, buffer).await
        }

        /// Send data to the peer.
        pub async fn send(&self, buf: &[u8]) -> std::io::Result<usize> {
            self.inner.send_to(buf, self.peer).await
        }

        /// Send several datagrams to the peer.
        ///
        /// There is no `sendmmsg` off Linux, so this sends one at a time and
        /// exists only to keep [`BatchSender::flush`] platform-agnostic. It
        /// reports how many datagrams were accepted before the first error, so
        /// the caller's resend bookkeeping is identical on both paths.
        pub async fn send_batch(&self, bufs: &[&[u8]]) -> std::io::Result<usize> {
            let mut sent = 0;
            for buf in bufs {
                match self.inner.send_to(buf, self.peer).await {
                    Ok(_) => sent += 1,
                    // Mirror `sendmmsg`: once at least one datagram is away, a
                    // failure is reported as a short send, not an error. The
                    // caller retries the remainder and will surface the error
                    // then if it persists.
                    Err(_) if sent > 0 => break,
                    Err(e) => return Err(e),
                }
            }
            Ok(sent)
        }

        /// Try to send data without blocking.
        #[allow(dead_code)]
        pub fn try_send(&self, buf: &[u8]) -> std::io::Result<usize> {
            self.inner.try_send_to(buf, self.peer)
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

    /// Receive a datagram from any tokio `UdpSocket` (single-packet fallback
    /// on non-Linux).
    pub async fn recv_udp_batch(
        socket: &UdpSocket,
        buffer: &mut RecvMmsgBuffer,
    ) -> std::io::Result<usize> {
        match socket.recv_from(&mut buffer.buffer).await {
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

        pub fn iter_payloads(&self) -> impl Iterator<Item = &'_ [u8]> {
            self.iter().map(|(_, data)| data)
        }

        #[cfg(test)]
        pub fn len(&self) -> usize {
            if self.has_packet { 1 } else { 0 }
        }

        #[cfg(test)]
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
pub use fallback_impl::{BatchUdpSocket, RecvMmsgBuffer, recv_udp_batch};
#[cfg(target_os = "linux")]
pub use unix_impl::{BatchUdpSocket, RecvMmsgBuffer, recv_udp_batch};

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
