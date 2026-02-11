use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};

static NS_COUNTER: AtomicU32 = AtomicU32::new(0);

/// Returns `true` if the environment supports namespace-based tests
/// (requires `ip` tool and passwordless `sudo`).
pub fn check_privileges() -> bool {
    let has_ip = Command::new("ip")
        .arg("netns")
        .output()
        .is_ok_and(|o| o.status.success());

    has_ip
        && Command::new("sudo")
            .args(["-n", "ip", "netns", "list"])
            .output()
            .is_ok_and(|o| o.status.success())
}

/// Generate a unique namespace/interface name safe for parallel tests.
///
/// Combines prefix + PID + atomic counter, truncated to 15 chars
/// (Linux netdev name limit).
pub fn unique_ns_name(prefix: &str) -> String {
    let seq = NS_COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id() % 0xffff;
    let name = format!("{prefix}_{pid:x}_{seq}");
    if name.len() > 15 { name[..15].to_string() } else { name }
}
