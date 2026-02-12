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
/// Combines prefix + PID + atomic counter. The uniqueness suffix
/// (`_{pid:x}_{seq}`) is always preserved; the prefix is truncated
/// if the total would exceed 15 chars (Linux netdev name limit).
pub fn unique_ns_name(prefix: &str) -> String {
    let seq = NS_COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id() % 0xffff;
    let suffix = format!("_{pid:x}_{seq}");
    let max_prefix = 15_usize.saturating_sub(suffix.len());
    let truncated_prefix = &prefix[..prefix.len().min(max_prefix)];
    format!("{truncated_prefix}{suffix}")
}
