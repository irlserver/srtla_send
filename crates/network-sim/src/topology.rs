use std::process::{Command, Output};

use anyhow::{Context, Result, bail};
use tracing::debug;

/// A Linux network namespace with RAII cleanup.
///
/// Creates the namespace on construction, brings up loopback, and deletes
/// it on drop. All commands inside the namespace run via `sudo ip netns exec`.
pub struct Namespace {
    pub name: String,
}

impl Namespace {
    pub fn new(name: &str) -> Result<Self> {
        // Clean up stale namespace with same name (idempotent)
        let _ = sudo(&["ip", "netns", "del", name]);

        sudo_checked(&["ip", "netns", "add", name])
            .with_context(|| format!("create netns '{name}'"))?;

        debug!(ns = name, "created network namespace");

        // Loopback — best-effort, failure is non-fatal
        let _ = sudo(&["ip", "netns", "exec", name, "ip", "link", "set", "lo", "up"]);

        Ok(Self {
            name: name.to_string(),
        })
    }

    /// Run a command inside this namespace, returning raw output.
    pub fn exec(&self, cmd: &str, args: &[&str]) -> Result<Output> {
        let mut full_args = vec!["ip", "netns", "exec", &self.name, cmd];
        full_args.extend_from_slice(args);
        sudo(&full_args).with_context(|| format!("exec '{cmd}' in ns '{}'", self.name))
    }

    /// Run a command inside this namespace, failing if it exits non-zero.
    pub fn exec_checked(&self, cmd: &str, args: &[&str]) -> Result<Output> {
        let mut full_args = vec!["ip", "netns", "exec", &self.name, cmd];
        full_args.extend_from_slice(args);
        sudo_checked(&full_args).with_context(|| format!("exec '{cmd}' in ns '{}'", self.name))
    }

    /// Create a veth pair connecting this namespace to `peer`.
    ///
    /// Each end gets an IP address assigned and is brought up.
    /// Interface names must be <= 15 chars (Linux limit).
    pub fn add_veth_link(
        &self,
        peer: &Namespace,
        local_iface: &str,
        peer_iface: &str,
        local_ip: &str,
        peer_ip: &str,
    ) -> Result<()> {
        // Clean up stale veth (idempotent)
        let _ = sudo(&["ip", "link", "del", local_iface]);

        // Create pair in host namespace
        sudo_checked(&[
            "ip",
            "link",
            "add",
            local_iface,
            "type",
            "veth",
            "peer",
            "name",
            peer_iface,
        ])
        .context("create veth pair")?;

        debug!(local = local_iface, peer = peer_iface, "created veth pair");

        // Move each end into its namespace
        sudo_checked(&["ip", "link", "set", local_iface, "netns", &self.name])
            .context("move local veth")?;
        sudo_checked(&["ip", "link", "set", peer_iface, "netns", &peer.name])
            .context("move peer veth")?;

        // Configure local end
        self.exec_checked("ip", &["addr", "add", local_ip, "dev", local_iface])
            .context("set local IP")?;
        self.exec_checked("ip", &["link", "set", local_iface, "up"])
            .context("bring local link up")?;

        // Configure peer end
        peer.exec_checked("ip", &["addr", "add", peer_ip, "dev", peer_iface])
            .context("set peer IP")?;
        peer.exec_checked("ip", &["link", "set", peer_iface, "up"])
            .context("bring peer link up")?;

        debug!(
            ns_local = self.name,
            ns_peer = peer.name,
            local_ip,
            peer_ip,
            "veth link configured"
        );

        Ok(())
    }
}

impl Drop for Namespace {
    fn drop(&mut self) {
        debug!(ns = self.name, "deleting network namespace");
        let _ = sudo(&["ip", "netns", "del", &self.name]);
    }
}

// -- helpers --

/// Run `sudo <args>`, returning raw output.
fn sudo(args: &[&str]) -> Result<Output> {
    Command::new("sudo")
        .args(args)
        .output()
        .with_context(|| format!("sudo {}", args.join(" ")))
}

/// Run `sudo <args>`, returning output on success or bailing with stderr.
fn sudo_checked(args: &[&str]) -> Result<Output> {
    let output = sudo(args)?;
    if !output.status.success() {
        bail!(
            "command failed: sudo {}\n{}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::{check_privileges, unique_ns_name};

    #[test]
    fn test_namespace_has_loopback() {
        if !check_privileges() {
            eprintln!("Skipping: insufficient privileges");
            return;
        }

        let ns = Namespace::new(&unique_ns_name("nst_a")).expect("create ns");
        let out = ns.exec("ip", &["link"]).expect("ip link");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(stdout.contains("lo"), "loopback missing: {stdout}");
    }

    #[test]
    fn test_veth_ping() {
        if !check_privileges() {
            eprintln!("Skipping: insufficient privileges");
            return;
        }

        let ns1 = Namespace::new(&unique_ns_name("nst_a")).expect("create ns1");
        let ns2 = Namespace::new(&unique_ns_name("nst_b")).expect("create ns2");

        let id = std::process::id() % 100_000;
        let v_a = format!("va_{id}");
        let v_b = format!("vb_{id}");

        ns1.add_veth_link(&ns2, &v_a, &v_b, "10.200.1.1/24", "10.200.1.2/24")
            .expect("add veth link");

        let out = ns1
            .exec("ping", &["-c", "1", "-W", "1", "10.200.1.2"])
            .expect("ping");

        assert!(
            out.status.success(),
            "ping failed:\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}
