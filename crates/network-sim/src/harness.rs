//! Test harness for end-to-end SRTLA integration tests.
//!
//! Provides [`SrtlaTestTopology`] for network namespace setup,
//! [`NamespaceProcess`] for managed child processes inside namespaces,
//! and [`SrtlaTestStack`] for the full 3-process test pipeline
//! (srt-live-transmit + srtla_rec + srtla_send).

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use crate::impairment::{ImpairmentConfig, apply_impairment};
use crate::test_util::unique_ns_name;
use crate::topology::Namespace;

// ---------------------------------------------------------------------------
// Dependency checking
// ---------------------------------------------------------------------------

/// Check if a binary exists in PATH.
pub fn check_binary(name: &str) -> Option<PathBuf> {
    Command::new("sh")
        .args(["-c", &format!("command -v {name}")])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| PathBuf::from(String::from_utf8_lossy(&o.stdout).trim().to_string()))
}

/// Reason why integration tests must be skipped.
#[derive(Debug)]
pub enum SkipReason {
    NotRoot,
    MissingBinary(String),
    MissingTool(String),
    NoNetem,
}

impl std::fmt::Display for SkipReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SkipReason::NotRoot => write!(f, "requires root / passwordless sudo"),
            SkipReason::MissingBinary(b) => write!(f, "{b} not found in PATH"),
            SkipReason::MissingTool(t) => write!(f, "system tool '{t}' not found"),
            SkipReason::NoNetem => write!(
                f,
                "sch_netem kernel module not available (try: sudo modprobe sch_netem)"
            ),
        }
    }
}

/// Check all dependencies needed for integration tests.
///
/// Returns `Ok(())` if everything is available, or `Err(SkipReason)` with
/// the first missing dependency.
pub fn check_integration_deps() -> std::result::Result<(), SkipReason> {
    // Root / sudo check
    if !crate::test_util::check_privileges() {
        return Err(SkipReason::NotRoot);
    }

    // External binaries
    for bin in &["srtla_rec", "srt-live-transmit"] {
        if check_binary(bin).is_none() {
            return Err(SkipReason::MissingBinary(bin.to_string()));
        }
    }

    // System tools
    for tool in &["ip", "tc", "ss"] {
        if check_binary(tool).is_none() {
            return Err(SkipReason::MissingTool(tool.to_string()));
        }
    }

    Ok(())
}

/// Check deps including netem (for tests that apply impairment).
pub fn check_impairment_deps() -> std::result::Result<(), SkipReason> {
    check_integration_deps()?;

    // Try to load sch_netem and check if it succeeded
    let modprobe_ok = Command::new("sudo")
        .args(["modprobe", "sch_netem"])
        .output()
        .is_ok_and(|o| o.status.success());

    if !modprobe_ok {
        return Err(SkipReason::NoNetem);
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// NamespaceProcess
// ---------------------------------------------------------------------------

/// A child process running inside a network namespace.
///
/// Captures stdout+stderr and kills the process on drop.
pub struct NamespaceProcess {
    child: Child,
    #[expect(dead_code)]
    label: String,
}

impl NamespaceProcess {
    /// Spawn `binary args...` inside `ns` via `sudo ip netns exec`.
    pub fn spawn(ns: &Namespace, binary: &str, args: &[&str]) -> Result<Self> {
        Self::spawn_with_env(ns, binary, args, &[])
    }

    /// Spawn with additional environment variables as `(key, value)` pairs.
    pub fn spawn_with_env(
        ns: &Namespace,
        binary: &str,
        args: &[&str],
        env: &[(&str, &str)],
    ) -> Result<Self> {
        let label = format!("{binary} in ns:{}", ns.name);
        let mut cmd = Command::new("sudo");
        cmd.args(["ip", "netns", "exec", &ns.name]);
        if !env.is_empty() {
            // Use `env` to set variables inside the namespace
            cmd.arg("env");
            for &(k, v) in env {
                cmd.arg(format!("{k}={v}"));
            }
        }
        cmd.arg(binary)
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let child = cmd.spawn().with_context(|| format!("spawn {label}"))?;

        tracing::debug!(%label, pid = child.id(), "spawned namespace process");
        Ok(Self { child, label })
    }

    /// Read all captured stdout lines (non-blocking snapshot via `try_wait`).
    /// Only meaningful after the process has exited.
    pub fn stdout_lines(&mut self) -> Vec<String> {
        match self.child.stdout.take() {
            Some(stdout) => BufReader::new(stdout)
                .lines()
                .map_while(|l| l.ok())
                .collect(),
            None => vec![],
        }
    }

    /// Read all captured stderr lines. Only meaningful after exit.
    pub fn stderr_lines(&mut self) -> Vec<String> {
        match self.child.stderr.take() {
            Some(stderr) => BufReader::new(stderr)
                .lines()
                .map_while(|l| l.ok())
                .collect(),
            None => vec![],
        }
    }

    /// Send SIGTERM, wait briefly, then SIGKILL if needed.
    ///
    /// Signals the entire process group (negative PID) so the inner process
    /// receives the signal even when wrapped by `sudo ip netns exec`.
    pub fn kill(&mut self) {
        if let Some(pid) = self.pid() {
            // Signal the entire process group so the inner process receives it
            let _ = Command::new("sudo")
                .args(["kill", "-TERM", "--", &format!("-{pid}")])
                .output();
        }

        match self.child.try_wait().ok().flatten() {
            Some(_) => return,
            None => {
                // Wait up to 2s for graceful exit
                std::thread::sleep(Duration::from_secs(2));
                if self.child.try_wait().ok().flatten().is_some() {
                    return;
                }
            }
        }

        // Force kill the process group
        if let Some(pid) = self.pid() {
            let _ = Command::new("sudo")
                .args(["kill", "-9", "--", &format!("-{pid}")])
                .output();
        }
        let _ = self.child.wait();
    }

    /// Check if the process is still running.
    pub fn is_alive(&mut self) -> bool {
        self.child.try_wait().ok().flatten().is_none()
    }

    /// If the process has exited, return its exit code and stderr.
    /// Returns `None` if still running.
    pub fn check_exit(&mut self) -> Option<(Option<i32>, String)> {
        match self.child.try_wait() {
            Ok(Some(status)) => {
                let stderr = self.stderr_lines().join("\n");
                Some((status.code(), stderr))
            }
            _ => None,
        }
    }

    fn pid(&self) -> Option<u32> {
        Some(self.child.id())
    }
}

impl Drop for NamespaceProcess {
    fn drop(&mut self) {
        self.kill();
    }
}

// ---------------------------------------------------------------------------
// SrtlaTestTopology
// ---------------------------------------------------------------------------

/// Network topology with sender and receiver namespaces connected by N veth links.
pub struct SrtlaTestTopology {
    pub sender_ns: Namespace,
    pub receiver_ns: Namespace,
    /// Sender-side IPs, e.g. `["10.10.1.1", "10.10.2.1"]`.
    pub sender_ips: Vec<String>,
    /// Receiver-side IP on the first link (used for srtla_rec bind address).
    pub receiver_ip: String,
    /// Sender-side veth interface names (for applying impairment).
    pub sender_ifaces: Vec<String>,
    /// Receiver-side veth interface names.
    pub receiver_ifaces: Vec<String>,
}

impl SrtlaTestTopology {
    /// Create a new topology with `num_links` veth pairs.
    ///
    /// Addresses use `10.10.{i+1}.{1|2}/24` where `i` is the link index.
    pub fn new(test_name: &str, num_links: usize) -> Result<Self> {
        assert!(num_links > 0, "need at least one link");

        let sender_ns = Namespace::new(&unique_ns_name(&format!("{test_name}_s")))?;
        let receiver_ns = Namespace::new(&unique_ns_name(&format!("{test_name}_r")))?;

        let mut sender_ips = Vec::with_capacity(num_links);
        let mut sender_ifaces = Vec::with_capacity(num_links);
        let mut receiver_ifaces = Vec::with_capacity(num_links);

        let pid = std::process::id() % 0xffff;

        for i in 0..num_links {
            let subnet = i + 1;
            let s_ip = format!("10.10.{subnet}.1");
            let r_ip = format!("10.10.{subnet}.2");
            let s_iface = format!("vs{pid:x}_{i}");
            let r_iface = format!("vr{pid:x}_{i}");

            // Truncate to 15 chars (Linux netdev limit)
            let s_iface = if s_iface.len() > 15 {
                s_iface[..15].to_string()
            } else {
                s_iface
            };
            let r_iface = if r_iface.len() > 15 {
                r_iface[..15].to_string()
            } else {
                r_iface
            };

            sender_ns.add_veth_link(
                &receiver_ns,
                &s_iface,
                &r_iface,
                &format!("{s_ip}/24"),
                &format!("{r_ip}/24"),
            )?;

            sender_ips.push(s_ip);
            sender_ifaces.push(s_iface);
            receiver_ifaces.push(r_iface);
        }

        let receiver_ip = "10.10.1.2".to_string();

        Ok(Self {
            sender_ns,
            receiver_ns,
            sender_ips,
            receiver_ip,
            sender_ifaces,
            receiver_ifaces,
        })
    }

    /// Apply impairment to sender-side veth link at `idx`.
    pub fn impair_link(&self, idx: usize, config: ImpairmentConfig) -> Result<()> {
        let iface = self
            .sender_ifaces
            .get(idx)
            .with_context(|| format!("link index {idx} out of range"))?;
        apply_impairment(&self.sender_ns, iface, config)
    }

    /// Write sender IPs to a temp file and return the path.
    pub fn write_ip_list(&self) -> Result<PathBuf> {
        let dir = tempfile::tempdir().context("create temp dir for IP list")?;
        let path = dir.keep().join("srtla_ips.txt");
        std::fs::write(&path, self.sender_ips.join("\n") + "\n").context("write IP list")?;
        Ok(path)
    }
}

// ---------------------------------------------------------------------------
// Waiting helpers
// ---------------------------------------------------------------------------

/// Poll `ss -uln` inside `ns` until `port` appears as a UDP listener.
pub fn wait_for_udp_listener(ns: &Namespace, port: u16, timeout: Duration) -> Result<()> {
    let start = Instant::now();
    let port_str = format!(":{port}");
    let mut last_ss_output;

    loop {
        let out = ns.exec("ss", &["-uln"])?;
        let stdout = String::from_utf8_lossy(&out.stdout);
        if stdout.lines().any(|line| line.contains(&port_str)) {
            return Ok(());
        }
        last_ss_output = stdout.to_string();

        if start.elapsed() > timeout {
            bail!(
                "timeout waiting for UDP listener on port {port} in ns {}\nlast ss -uln \
                 output:\n{last_ss_output}",
                ns.name
            );
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

// ---------------------------------------------------------------------------
// SrtlaTestStack
// ---------------------------------------------------------------------------

/// Full 3-process SRTLA test stack: srt-live-transmit + srtla_rec + srtla_send.
pub struct SrtlaTestStack {
    pub topo: SrtlaTestTopology,
    srt_server: Option<NamespaceProcess>,
    srtla_rec: Option<NamespaceProcess>,
    srtla_send: Option<NamespaceProcess>,
    _ip_list_path: PathBuf,
}

/// Output collected from all processes after stopping the stack.
pub struct StackOutput {
    pub srt_server_stdout: Vec<String>,
    pub srt_server_stderr: Vec<String>,
    pub srtla_rec_stdout: Vec<String>,
    pub srtla_rec_stderr: Vec<String>,
    pub srtla_send_stdout: Vec<String>,
    pub srtla_send_stderr: Vec<String>,
}

/// Ports used by the test stack.
const SRT_SERVER_PORT: u16 = 4001;
const SRTLA_REC_PORT: u16 = 5000;
const SRTLA_SEND_SRT_PORT: u16 = 5555;

impl SrtlaTestStack {
    /// Start the full stack: srt-live-transmit → srtla_rec → srtla_send.
    ///
    /// `sender_extra_args` are appended to the srtla_send command line.
    pub fn start(test_name: &str, num_links: usize, sender_extra_args: &[&str]) -> Result<Self> {
        let topo = SrtlaTestTopology::new(test_name, num_links)?;
        let ip_list_path = topo.write_ip_list()?;

        // 1. Start srt-live-transmit in receiver NS
        //    Acts as an SRT listener that sinks to /dev/null.
        let srt_uri = format!("srt://:{}?mode=listener", SRT_SERVER_PORT);
        // Sink to a UDP port — srt-live-transmit only supports srt://, udp://, file://con
        let sink_uri = "udp://127.0.0.1:9999";
        let mut srt_server = NamespaceProcess::spawn(
            &topo.receiver_ns,
            "srt-live-transmit",
            &[&srt_uri, sink_uri],
        )
        .context("start srt-live-transmit")?;

        // Brief pause for listener setup, then check it's alive
        std::thread::sleep(Duration::from_millis(500));
        if let Some((code, stderr)) = srt_server.check_exit() {
            bail!("srt-live-transmit exited immediately (code: {code:?})\nstderr:\n{stderr}");
        }
        wait_for_udp_listener(&topo.receiver_ns, SRT_SERVER_PORT, Duration::from_secs(5))
            .context("wait for srt-live-transmit")?;

        // 2. Start srtla_rec in receiver NS
        let srtla_port_str = SRTLA_REC_PORT.to_string();
        let srt_port_str = SRT_SERVER_PORT.to_string();
        let mut srtla_rec = NamespaceProcess::spawn(
            &topo.receiver_ns,
            "srtla_rec",
            &[
                "--srtla_port",
                &srtla_port_str,
                "--srt_hostname",
                "127.0.0.1",
                "--srt_port",
                &srt_port_str,
            ],
        )
        .context("start srtla_rec")?;

        // Wait for srtla_rec to be listening
        std::thread::sleep(Duration::from_millis(500));
        if let Some((code, stderr)) = srtla_rec.check_exit() {
            bail!("srtla_rec exited immediately (code: {code:?})\nstderr:\n{stderr}");
        }
        wait_for_udp_listener(&topo.receiver_ns, SRTLA_REC_PORT, Duration::from_secs(5))
            .context("wait for srtla_rec")?;

        // 3. Start srtla_send in sender NS
        let send_port = SRTLA_SEND_SRT_PORT.to_string();
        let rec_port = SRTLA_REC_PORT.to_string();
        let ip_list_str = ip_list_path.to_string_lossy().to_string();

        let mut send_args = vec![
            send_port.as_str(),
            topo.receiver_ip.as_str(),
            rec_port.as_str(),
            ip_list_str.as_str(),
        ];
        send_args.extend_from_slice(sender_extra_args);

        // Find the srtla_send binary built by cargo
        let srtla_send_bin = find_srtla_send_binary()?;
        let bin_str = srtla_send_bin.to_string_lossy().to_string();

        let srtla_send = NamespaceProcess::spawn_with_env(
            &topo.sender_ns,
            &bin_str,
            &send_args,
            &[("RUST_LOG", "debug")],
        )
        .context("start srtla_send")?;

        Ok(Self {
            topo,
            srt_server: Some(srt_server),
            srtla_rec: Some(srtla_rec),
            srtla_send: Some(srtla_send),
            _ip_list_path: ip_list_path,
        })
    }

    /// Apply impairment to sender-side link at `idx`.
    pub fn impair_link(&self, idx: usize, config: ImpairmentConfig) -> Result<()> {
        self.topo.impair_link(idx, config)
    }

    /// The local SRT port that srtla_send listens on (for injecting test data).
    pub fn sender_srt_port(&self) -> u16 {
        SRTLA_SEND_SRT_PORT
    }

    /// Stop all processes and collect their output.
    pub fn stop(&mut self) -> StackOutput {
        let mut send_out = (vec![], vec![]);
        let mut rec_out = (vec![], vec![]);
        let mut srt_out = (vec![], vec![]);

        // Kill in reverse order: sender → receiver → srt server
        if let Some(mut p) = self.srtla_send.take() {
            p.kill();
            send_out = (p.stdout_lines(), p.stderr_lines());
        }
        if let Some(mut p) = self.srtla_rec.take() {
            p.kill();
            rec_out = (p.stdout_lines(), p.stderr_lines());
        }
        if let Some(mut p) = self.srt_server.take() {
            p.kill();
            srt_out = (p.stdout_lines(), p.stderr_lines());
        }

        StackOutput {
            srt_server_stdout: srt_out.0,
            srt_server_stderr: srt_out.1,
            srtla_rec_stdout: rec_out.0,
            srtla_rec_stderr: rec_out.1,
            srtla_send_stdout: send_out.0,
            srtla_send_stderr: send_out.1,
        }
    }
}

impl Drop for SrtlaTestStack {
    fn drop(&mut self) {
        // Ensure all processes are killed even if stop() wasn't called.
        // Dropping NamespaceProcess triggers its Drop impl which calls kill().
        drop(self.srtla_send.take());
        drop(self.srtla_rec.take());
        drop(self.srt_server.take());
    }
}

// ---------------------------------------------------------------------------
// UDP injection
// ---------------------------------------------------------------------------

/// Inject `count` UDP packets into a port inside `ns`.
///
/// Sends from within the namespace using a bound local socket. Each packet
/// is 188 bytes (MPEG-TS packet size) of zeroes to simulate SRT data.
pub fn inject_udp_packets(ns: &Namespace, target_ip: &str, port: u16, count: usize) -> Result<()> {
    let addr = format!("{target_ip}:{port}");
    let script = format!(
        "import socket; s=socket.socket(socket.AF_INET,socket.SOCK_DGRAM); \
         [s.sendto(b'\\x00'*188,('{target_ip}',{port})) for _ in range({count})]; s.close()"
    );
    ns.exec_checked("python3", &["-c", &script])
        .with_context(|| format!("inject {count} UDP packets to {addr}"))?;
    Ok(())
}

/// Inject UDP packets at a steady rate (packets/sec) for `duration`.
pub fn inject_udp_stream(
    ns: &Namespace,
    target_ip: &str,
    port: u16,
    packets_per_sec: u32,
    duration: Duration,
) -> Result<()> {
    if packets_per_sec == 0 {
        bail!("packets_per_sec must be > 0");
    }
    let interval_us = 1_000_000 / packets_per_sec;
    let dur_secs = duration.as_secs_f64();

    let script = format!(
        "import socket,time\ns=socket.socket(socket.AF_INET,socket.SOCK_DGRAM)\nd=b'\\x00'*188\\nstart=time.time(); i=0\nwhile time.time()-start<{dur_secs}:\n\x20 \
         s.sendto(d,('{target_ip}',{port}))\n\x20 i+=1\n\x20 \
         time.sleep({interval_us}/1e6)\ns.close()\nprint(f'sent {{i}} packets')"
    );
    ns.exec_checked("python3", &["-c", &script])
        .context("inject UDP stream")?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Locate the srtla_send binary from a cargo build.
fn find_srtla_send_binary() -> Result<PathBuf> {
    // Check common cargo build output locations
    let candidates = [
        // Debug build
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("target/debug/srtla_send"),
        // Release build
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("target/release/srtla_send"),
    ];

    for path in &candidates {
        if path.exists() {
            return Ok(path.clone());
        }
    }

    // Fall back to PATH
    check_binary("srtla_send").ok_or_else(|| {
        anyhow::anyhow!(
            "srtla_send binary not found. Run `cargo build` first. Checked: {:?}",
            candidates
        )
    })
}
