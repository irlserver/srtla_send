//! Egress binding for Apple platforms (iOS, iPadOS, tvOS, macOS).
//!
//! Darwin does not do source-based routing. Binding a socket to an uplink's
//! source IP  what [`SourceIpBinder`](super::SourceIpBinder) does, and what is
//! sufficient on a policy-routed Linux host  fixes the source address in the
//! IP header but leaves egress selection to the routing table, so every uplink
//! still leaves over the default route. Bonding a cellular link against wifi
//! that way silently sends all traffic out one interface.
//!
//! The Darwin mechanism that actually steers egress is the `IP_BOUND_IF` /
//! `IPV6_BOUND_IF` socket option: it pins the socket to an interface index, and
//! the stack then routes through that interface regardless of the default
//! route. [`AppleInterfaceBinder`] sets it before `bind`/`connect`, then binds
//! the source IP as well so the uplink's `IpAddr` identity (which the whole
//! SRTLA core is keyed on) still matches the 5-tuple the receiver observes.
//!
//! # Host responsibilities on iOS
//!
//! `IP_BOUND_IF` selects among interfaces that are *already up*. It does not
//! activate one. Two things remain the embedding app's job:
//!
//! 1. **Bring cellular up and hold it there.** iOS tears down `pdp_ip0` when
//!    nothing wants it, and an interface with no address cannot be resolved
//!    here. The app must hold an `NWConnection` (or `NWPathMonitor`) with
//!    `requiredInterfaceType = .cellular` open for as long as the bonded
//!    session runs. Without it, the cellular uplink either fails to resolve at
//!    startup or disappears mid-stream.
//! 2. **Feed the current uplink addresses in.** iOS reassigns `pdp_ip0`'s
//!    address across radio transitions. The app should watch `NWPathMonitor`
//!    and push the new address list through the existing IP-reload path; each
//!    uplink is then rebound through this binder with its new address.
//!
//! When the host already knows the interface name from `NWPath`
//! (`nw_interface_get_name`), pass it via [`AppleInterfaceBinder::with_interface`]
//! to skip the `getifaddrs` lookup and remove the ambiguity window during an
//! address change.

use std::collections::HashMap;
use std::ffi::CString;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV6};
use std::os::fd::AsRawFd;

use anyhow::{Context, Result, anyhow, bail};
use socket2::Socket;
use tracing::debug;

use super::UplinkBinder;

/// Binder that pins each uplink socket to a network interface with
/// `IP_BOUND_IF` / `IPV6_BOUND_IF`, then binds the uplink source IP.
///
/// The interface is resolved from the uplink's `IpAddr` by scanning
/// `getifaddrs` unless the host registered an explicit name for that address
/// via [`with_interface`](Self::with_interface).
#[derive(Debug, Default, Clone)]
pub struct AppleInterfaceBinder {
    /// Host-supplied `uplink IP -> interface name` overrides, e.g.
    /// `10.x.x.x -> "pdp_ip0"` taken straight from `NWPath`.
    overrides: HashMap<IpAddr, String>,
}

impl AppleInterfaceBinder {
    /// A binder that resolves every uplink's interface from the system
    /// interface list.
    pub fn new() -> Self {
        Self::default()
    }

    /// Pin `ip` to the named interface instead of resolving it.
    ///
    /// Use this when the host already knows the mapping (on iOS,
    /// `nw_interface_get_name` on the `NWPath`'s interface). The name is not
    /// validated here; an unknown interface fails at bind time, when the error
    /// can be attributed to the uplink.
    #[must_use]
    pub fn with_interface(mut self, ip: IpAddr, ifname: impl Into<String>) -> Self {
        self.overrides.insert(ip, ifname.into());
        self
    }
}

impl UplinkBinder for AppleInterfaceBinder {
    fn bind(&self, sock: &Socket, ip: IpAddr) -> Result<()> {
        let ifname = match self.overrides.get(&ip) {
            Some(name) => name.clone(),
            None => interface_name_for_ip(ip)
                .with_context(|| format!("no interface carries uplink address {ip}"))?,
        };
        let index = interface_index(&ifname)
            .with_context(|| format!("resolve interface index for {ifname}"))?;

        bind_to_interface_index(sock, ip, index)
            .with_context(|| format!("bind uplink {ip} to {ifname} (index {index})"))?;

        // A link-local source address is only unambiguous with the interface
        // scope attached; the kernel rejects an unscoped bind for fe80::/10.
        let local = match ip {
            IpAddr::V6(v6) if is_unicast_link_local(v6) => {
                SocketAddr::V6(SocketAddrV6::new(v6, 0, 0, index))
            }
            other => SocketAddr::new(other, 0),
        };
        sock.bind(&local.into())
            .with_context(|| format!("bind socket to source address {ip}"))?;

        debug!(
            "uplink {} bound to interface {} (index {})",
            ip, ifname, index
        );
        Ok(())
    }
}

/// Apply `IP_BOUND_IF` (v4) or `IPV6_BOUND_IF` (v6) to `sock`.
///
/// Must run before `bind`/`connect`: Darwin resolves the route at connect time,
/// so setting the option afterwards leaves the socket on the default route.
fn bind_to_interface_index(sock: &Socket, ip: IpAddr, index: u32) -> Result<()> {
    let (level, name) = match ip {
        IpAddr::V4(_) => (libc::IPPROTO_IP, libc::IP_BOUND_IF),
        IpAddr::V6(_) => (libc::IPPROTO_IPV6, libc::IPV6_BOUND_IF),
    };
    // SAFETY: `sock` outlives the call, so the fd is valid. The option value is
    // a `u32` interface index, which is the width both options expect.
    let rc = unsafe {
        libc::setsockopt(
            sock.as_raw_fd(),
            level,
            name,
            std::ptr::addr_of!(index).cast::<libc::c_void>(),
            std::mem::size_of::<u32>() as libc::socklen_t,
        )
    };
    if rc != 0 {
        return Err(std::io::Error::last_os_error()).context("setsockopt BOUND_IF");
    }
    Ok(())
}

/// Look up an interface index by name. Index 0 is the "unspecified interface"
/// sentinel, which `if_nametoindex` also returns on failure, so it is an error
/// either way.
fn interface_index(ifname: &str) -> Result<u32> {
    let cname = CString::new(ifname).context("interface name contains a NUL byte")?;
    // SAFETY: `cname` is a valid NUL-terminated string that outlives the call.
    let index = unsafe { libc::if_nametoindex(cname.as_ptr()) };
    if index == 0 {
        return Err(std::io::Error::last_os_error()).context("if_nametoindex");
    }
    Ok(index)
}

/// Find the interface carrying `ip`.
///
/// Returns the first match. An address present on two interfaces is a
/// misconfiguration we cannot disambiguate; the host should pass an explicit
/// name via [`AppleInterfaceBinder::with_interface`] in that case.
pub fn interface_name_for_ip(ip: IpAddr) -> Result<String> {
    let list = IfAddrs::get()?;
    for entry in list.iter() {
        // SAFETY: `entry` points into the list, which is alive for this loop.
        let addr = unsafe { sockaddr_to_ip((*entry).ifa_addr) };
        if addr != Some(ip) {
            continue;
        }
        // SAFETY: as above; `ifa_name` is a NUL-terminated string owned by the
        // list, copied out here before the list is freed.
        let name = unsafe { std::ffi::CStr::from_ptr((*entry).ifa_name) };
        return name
            .to_str()
            .map(str::to_owned)
            .map_err(|_| anyhow!("interface name for {ip} is not valid UTF-8"));
    }
    bail!("address {ip} is not assigned to any interface")
}

/// Read an `IpAddr` out of a `sockaddr`, or `None` for a null pointer or a
/// family we do not care about (`AF_LINK` entries, which `getifaddrs` also
/// returns).
unsafe fn sockaddr_to_ip(sa: *const libc::sockaddr) -> Option<IpAddr> {
    if sa.is_null() {
        return None;
    }
    // SAFETY: caller guarantees `sa` points at a live `sockaddr`. Fields are
    // read unaligned because `getifaddrs` packs entries without regard for the
    // alignment of the larger `sockaddr_in6`.
    let family = unsafe { std::ptr::read_unaligned(std::ptr::addr_of!((*sa).sa_family)) };
    match c_int_from(family) {
        libc::AF_INET => {
            // SAFETY: family says this is a `sockaddr_in`.
            let v4 = unsafe { std::ptr::read_unaligned(sa.cast::<libc::sockaddr_in>()) };
            Some(IpAddr::V4(Ipv4Addr::from(u32::from_be(v4.sin_addr.s_addr))))
        }
        libc::AF_INET6 => {
            // SAFETY: family says this is a `sockaddr_in6`.
            let v6 = unsafe { std::ptr::read_unaligned(sa.cast::<libc::sockaddr_in6>()) };
            Some(IpAddr::V6(Ipv6Addr::from(v6.sin6_addr.s6_addr)))
        }
        _ => None,
    }
}

fn c_int_from(family: libc::sa_family_t) -> libc::c_int {
    libc::c_int::from(family)
}

/// fe80::/10, the range that needs a scope id to be routable.
fn is_unicast_link_local(addr: Ipv6Addr) -> bool {
    addr.segments()[0] & 0xffc0 == 0xfe80
}

/// Owned `getifaddrs` list. Exists so every early return still hits
/// `freeifaddrs`.
struct IfAddrs(*mut libc::ifaddrs);

impl IfAddrs {
    fn get() -> Result<Self> {
        let mut head: *mut libc::ifaddrs = std::ptr::null_mut();
        // SAFETY: `head` is a valid out-pointer; on success the list is ours to
        // free, which `Drop` does.
        let rc = unsafe { libc::getifaddrs(std::ptr::addr_of_mut!(head)) };
        if rc != 0 {
            return Err(std::io::Error::last_os_error()).context("getifaddrs");
        }
        Ok(Self(head))
    }

    fn iter(&self) -> IfAddrsIter<'_> {
        IfAddrsIter {
            next: self.0,
            _list: std::marker::PhantomData,
        }
    }
}

impl Drop for IfAddrs {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: `self.0` came from a successful `getifaddrs` and is freed
            // exactly once, here.
            unsafe { libc::freeifaddrs(self.0) };
        }
    }
}

struct IfAddrsIter<'a> {
    next: *mut libc::ifaddrs,
    _list: std::marker::PhantomData<&'a IfAddrs>,
}

impl Iterator for IfAddrsIter<'_> {
    type Item = *const libc::ifaddrs;

    fn next(&mut self) -> Option<Self::Item> {
        if self.next.is_null() {
            return None;
        }
        let current = self.next;
        // SAFETY: `current` is a live list node borrowed from the owning
        // `IfAddrs`, so reading its `ifa_next` link is valid.
        self.next = unsafe { (*current).ifa_next };
        Some(current.cast_const())
    }
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;

    use socket2::{Domain, Protocol, Type};

    use super::*;

    #[test]
    fn loopback_resolves_to_an_interface() {
        let name = interface_name_for_ip(IpAddr::V4(Ipv4Addr::LOCALHOST))
            .expect("loopback address must be assigned to an interface");
        assert!(!name.is_empty());
        assert!(interface_index(&name).expect("loopback interface has an index") > 0);
    }

    #[test]
    fn unassigned_address_does_not_resolve() {
        // TEST-NET-1 (RFC 5737), never assigned to a real interface.
        let unassigned = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1));
        assert!(interface_name_for_ip(unassigned).is_err());
    }

    #[test]
    fn unknown_interface_name_is_an_error() {
        assert!(interface_index("nosuchif0").is_err());
    }

    #[test]
    fn interface_name_with_nul_is_rejected() {
        assert!(interface_index("lo\0eth0").is_err());
    }

    #[test]
    fn binds_loopback_socket_to_its_interface() {
        let sock = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP)).unwrap();
        let ip = IpAddr::V4(Ipv4Addr::LOCALHOST);

        AppleInterfaceBinder::new()
            .bind(&sock, ip)
            .expect("loopback bind must succeed");

        let local = sock.local_addr().unwrap().as_socket().unwrap();
        assert_eq!(local.ip(), ip);
    }

    #[test]
    fn override_bypasses_address_lookup() {
        let sock = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP)).unwrap();
        let ip = IpAddr::V4(Ipv4Addr::LOCALHOST);
        let ifname = interface_name_for_ip(ip).unwrap();

        AppleInterfaceBinder::new()
            .with_interface(ip, ifname)
            .bind(&sock, ip)
            .expect("explicit interface bind must succeed");
    }

    #[test]
    fn override_with_unknown_interface_fails_at_bind() {
        let sock = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP)).unwrap();
        let ip = IpAddr::V4(Ipv4Addr::LOCALHOST);

        assert!(
            AppleInterfaceBinder::new()
                .with_interface(ip, "nosuchif0")
                .bind(&sock, ip)
                .is_err()
        );
    }

    #[test]
    fn link_local_detection() {
        assert!(is_unicast_link_local("fe80::1".parse().unwrap()));
        assert!(is_unicast_link_local("febf::1".parse().unwrap()));
        assert!(!is_unicast_link_local("fec0::1".parse().unwrap()));
        assert!(!is_unicast_link_local("2001:db8::1".parse().unwrap()));
    }
}
