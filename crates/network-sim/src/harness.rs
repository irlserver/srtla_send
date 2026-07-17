//! Test harness for end-to-end SRTLA integration tests.
//!
//! Provides [`SrtlaTestTopology`] for network namespace setup,
//! [`NamespaceProcess`] for managed child processes inside namespaces,
//! and [`SrtlaTestStack`] for the full 3-process test pipeline
//! (srt-live-transmit + srtla_rec + srtla_send).

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
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

/// C source for the adaptive SRT sender, compiled on demand. Kept out of
/// the Rust build on purpose: the workspace must build without libsrt
/// dev headers, and only a machine actually running the netns tests (a
/// superset of the srtla stack, which already needs libsrt) has to be
/// able to compile it.
const ADAPTIVE_SENDER_SRC: &str = include_str!("adaptive_srt_send.c");

/// Whether the adaptive SRT sender can be built here: a C compiler and
/// libsrt dev, the latter probed through `pkg-config srt`.
pub fn check_adaptive_sender_deps() -> std::result::Result<(), SkipReason> {
    if check_binary("cc").is_none() {
        return Err(SkipReason::MissingTool("cc".into()));
    }
    let srt_dev = Command::new("pkg-config")
        .args(["--exists", "srt"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !srt_dev {
        return Err(SkipReason::MissingBinary(
            "libsrt dev (pkg-config srt)".into(),
        ));
    }
    Ok(())
}

/// Compile the adaptive SRT sender against the system libsrt and return
/// the built binary. Gate on [`check_adaptive_sender_deps`] first.
///
/// Recompiles every call — the source is tiny and this keeps a stale
/// binary from surviving a source edit. The output lands in the system
/// temp dir, not the cargo target tree.
pub fn build_adaptive_sender() -> Result<PathBuf> {
    let dir = std::env::temp_dir();
    let src = dir.join("network_sim_adaptive_srt_send.c");
    let bin = dir.join("network_sim_adaptive_srt_send");
    std::fs::write(&src, ADAPTIVE_SENDER_SRC).context("write adaptive sender source")?;

    let flags_out = Command::new("pkg-config")
        .args(["--cflags", "--libs", "srt"])
        .output()
        .context("pkg-config srt")?;
    if !flags_out.status.success() {
        bail!(
            "pkg-config srt failed: {}",
            String::from_utf8_lossy(&flags_out.stderr).trim()
        );
    }
    let flags = String::from_utf8_lossy(&flags_out.stdout);

    let mut args: Vec<String> = vec![
        src.to_string_lossy().into_owned(),
        "-O2".into(),
        "-o".into(),
        bin.to_string_lossy().into_owned(),
    ];
    args.extend(flags.split_whitespace().map(str::to_string));

    let out = Command::new("cc")
        .args(&args)
        .output()
        .context("cc adaptive sender")?;
    if !out.status.success() {
        bail!(
            "compiling adaptive sender failed:\n{}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(bin)
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
///
/// The output pipes are drained continuously by background threads, and
/// that is load-bearing rather than a convenience. A piped child whose
/// output nobody reads blocks in `write()` as soon as the 64 KiB pipe
/// buffer fills. srtla_send runs here with `RUST_LOG=debug`, which at a
/// few Mbps fills that buffer in about a second — and because the task
/// doing the logging is the main select loop, the *sender* wedges while
/// its control socket (which logs almost nothing) carries on answering.
/// The symptom is a stats snapshot frozen at the values it held a second
/// into the run, which reads like an impossibly stable control loop
/// rather than a deadlocked one. Short tests never noticed because they
/// finish under 64 KiB.
pub struct NamespaceProcess {
    child: Child,
    #[expect(dead_code)]
    label: String,
    stdout: Arc<Mutex<Vec<String>>>,
    stderr: Arc<Mutex<Vec<String>>>,
}

/// Drain a child pipe into a shared buffer, line by line, until EOF.
fn drain_pipe<R: std::io::Read + Send + 'static>(pipe: R, sink: Arc<Mutex<Vec<String>>>) {
    std::thread::spawn(move || {
        for line in BufReader::new(pipe).lines().map_while(|l| l.ok()) {
            if let Ok(mut buf) = sink.lock() {
                buf.push(line);
            }
        }
    });
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

        let mut child = cmd.spawn().with_context(|| format!("spawn {label}"))?;

        // Start draining immediately — see the note on the struct. If we
        // wait until the process exits to read these, it never gets there.
        let stdout = Arc::new(Mutex::new(Vec::new()));
        let stderr = Arc::new(Mutex::new(Vec::new()));
        if let Some(pipe) = child.stdout.take() {
            drain_pipe(pipe, Arc::clone(&stdout));
        }
        if let Some(pipe) = child.stderr.take() {
            drain_pipe(pipe, Arc::clone(&stderr));
        }

        tracing::debug!(%label, pid = child.id(), "spawned namespace process");
        Ok(Self {
            child,
            label,
            stdout,
            stderr,
        })
    }

    /// Snapshot of the stdout lines captured so far. Safe to call while
    /// the process is still running.
    pub fn stdout_lines(&mut self) -> Vec<String> {
        self.stdout.lock().map(|b| b.clone()).unwrap_or_default()
    }

    /// Snapshot of the stderr lines captured so far. Safe to call while
    /// the process is still running.
    pub fn stderr_lines(&mut self) -> Vec<String> {
        self.stderr.lock().map(|b| b.clone()).unwrap_or_default()
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

        // One receiver endpoint reached over every uplink, which is how
        // SRTLA actually bonds: N sender source IPs, one receiver
        // address. A per-link near-end IP (the old `10.10.1.2`) only
        // worked for link 0 — every other uplink routed its packets out
        // link 0's veth and got dropped by the receiver's reverse-path
        // filter, so it never registered and the bond was really one
        // link. See `wire_bonding_routes`.
        let receiver_ip = SRTLA_RECEIVER_SERVICE_IP.to_string();

        let topo = Self {
            sender_ns,
            receiver_ns,
            sender_ips,
            receiver_ip,
            sender_ifaces,
            receiver_ifaces,
        };
        topo.wire_bonding_routes()?;
        Ok(topo)
    }

    /// Make the single receiver endpoint reachable over each uplink
    /// independently, so every source IP egresses its own (impairable)
    /// veth. This is the piece that turns the topology from "one link
    /// plus dead spares" into a real bond.
    ///
    /// Per uplink `i` on subnet `10.10.{i+1}.0/24` (sender `.1`,
    /// receiver `.2`):
    ///
    /// - The receiver owns the service IP on `lo`, so it answers on it no
    ///   matter which veth a request arrived on.
    /// - The sender gets a routing table per source IP
    ///   (`ip rule from <src> lookup <table>`) whose route to the service
    ///   IP points out that uplink's veth. Binding a socket to the source
    ///   IP therefore pins its traffic to that veth.
    /// - The receiver returns packets to each sender source with the
    ///   service IP as source address (`src` on the route), so the
    ///   sender's connected UDP socket accepts the reply.
    /// - Reverse-path filtering is relaxed on both ends. With per-source
    ///   policy routing the return route lives outside the main table, so
    ///   strict rp_filter (the default) would drop the very packets that
    ///   make the uplink work.
    fn wire_bonding_routes(&self) -> Result<()> {
        disable_rp_filter(&self.sender_ns, &self.sender_ifaces)?;
        disable_rp_filter(&self.receiver_ns, &self.receiver_ifaces)?;

        // Service IP lives on the receiver's loopback.
        self.receiver_ns
            .exec_checked(
                "ip",
                &[
                    "addr",
                    "add",
                    &format!("{SRTLA_RECEIVER_SERVICE_IP}/32"),
                    "dev",
                    "lo",
                ],
            )
            .context("add receiver service IP")?;

        for (i, (s_ip, s_iface)) in self
            .sender_ips
            .iter()
            .zip(self.sender_ifaces.iter())
            .enumerate()
        {
            let subnet = i + 1;
            let r_ip = format!("10.10.{subnet}.2");
            let table = (subnet).to_string();

            // Sender: source-routed path to the service IP over this veth.
            self.sender_ns
                .exec_checked(
                    "ip",
                    &[
                        "route",
                        "add",
                        &format!("{SRTLA_RECEIVER_SERVICE_IP}/32"),
                        "via",
                        &r_ip,
                        "dev",
                        s_iface,
                        "table",
                        &table,
                    ],
                )
                .context("sender per-uplink route")?;
            self.sender_ns
                .exec_checked("ip", &["rule", "add", "from", s_ip, "lookup", &table])
                .context("sender per-uplink rule")?;

            // Receiver: return to this sender source with the service IP
            // as the source address, so the reply's peer matches what the
            // sender connected to.
            let r_iface = &self.receiver_ifaces[i];
            self.receiver_ns
                .exec_checked(
                    "ip",
                    &[
                        "route",
                        "add",
                        &format!("{s_ip}/32"),
                        "dev",
                        r_iface,
                        "src",
                        SRTLA_RECEIVER_SERVICE_IP,
                    ],
                )
                .context("receiver return route")?;
        }

        Ok(())
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

/// Poll `ss -uan` inside `ns` until at least `min_count` UDP sockets are
/// connected to `peer_ip:peer_port`. The sender `connect()`s one socket per
/// source IP to the receiver as it brings each uplink online, so a connected
/// peer entry is the observable readiness signal that replaces a fixed
/// registration sleep — it returns as soon as the state appears.
pub fn wait_for_connected_uplinks(
    ns: &Namespace,
    peer_ip: &str,
    peer_port: u16,
    min_count: usize,
    timeout: Duration,
) -> Result<()> {
    let start = Instant::now();
    let peer = format!("{peer_ip}:{peer_port}");
    let mut last_ss_output;

    loop {
        let out = ns.exec("ss", &["-uan"])?;
        let stdout = String::from_utf8_lossy(&out.stdout);
        let count = stdout.lines().filter(|line| line.contains(&peer)).count();
        if count >= min_count {
            return Ok(());
        }
        last_ss_output = stdout.to_string();

        if start.elapsed() > timeout {
            bail!(
                "timeout waiting for {min_count} connected uplink(s) to {peer} in ns {} (saw \
                 {count})\nlast ss -uan output:\n{last_ss_output}",
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
    srt_caller: Option<NamespaceProcess>,
    _ip_list_path: PathBuf,
}

/// UDP port the SRT caller ingests from, when one is started.
pub const SRT_CALLER_INGEST_PORT: u16 = 6000;

/// The single receiver endpoint every uplink connects to. Lives on the
/// receiver's loopback and is reachable over each veth via per-source
/// policy routing (see `SrtlaTestTopology::wire_bonding_routes`).
const SRTLA_RECEIVER_SERVICE_IP: &str = "10.99.0.1";

/// Disable reverse-path filtering in a namespace: set the `all` and
/// `default` keys plus every named interface to `0`. The effective value
/// is `max(all, iface)`, so both have to be cleared.
///
/// Off, not loose (`2`): whether loose mode honours the per-source policy
/// routing this topology relies on is kernel-version-dependent, and a
/// test namespace has nothing to protect, so remove the variable.
fn disable_rp_filter(ns: &Namespace, ifaces: &[String]) -> Result<()> {
    let mut keys = vec!["all".to_string(), "default".to_string()];
    keys.extend(ifaces.iter().cloned());
    for key in keys {
        // Best-effort per key: a kernel may lack a given conf path, and
        // that should not fail the whole topology.
        let _ = ns.exec(
            "sysctl",
            &["-w", &format!("net.ipv4.conf.{key}.rp_filter=0")],
        );
    }
    Ok(())
}

/// Output collected from all processes after stopping the stack.
pub struct StackOutput {
    pub srt_server_stdout: Vec<String>,
    pub srt_server_stderr: Vec<String>,
    pub srtla_rec_stdout: Vec<String>,
    pub srtla_rec_stderr: Vec<String>,
    pub srtla_send_stdout: Vec<String>,
    pub srtla_send_stderr: Vec<String>,
    pub srt_caller_stdout: Vec<String>,
    pub srt_caller_stderr: Vec<String>,
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
            srt_caller: None,
            _ip_list_path: ip_list_path,
        })
    }

    /// Start a real SRT caller in the sender namespace, in front of
    /// srtla_send.
    ///
    /// Without this the stack carries no SRT session: injecting raw UDP
    /// into srtla_send's listener gets it proxied over the bond, but the
    /// far end never completes a handshake, so it never returns ACKs or
    /// NAKs. Any test that depends on loss or RTT feedback reaching the
    /// sender — i.e. anything touching congestion control or link
    /// scoring — is silently vacuous without a caller here.
    ///
    /// With it, the chain is a genuine end-to-end SRT connection:
    ///
    /// ```text
    /// UDP :6000 → srt-live-transmit (caller) → srtla_send :5555
    ///     → [bonded uplinks] → srtla_rec → srt-live-transmit (listener)
    /// ```
    ///
    /// so the listener's ACK/NAK stream flows back through the bond and
    /// drives the real feedback path. Feed it with
    /// [`inject_udp_stream`] on [`SRT_CALLER_INGEST_PORT`].
    pub fn start_srt_caller(&mut self) -> Result<()> {
        let in_uri = format!("udp://:{SRT_CALLER_INGEST_PORT}");
        // Latency must sit *above* the TBF buffer depth (`latency 1s` in
        // impairment.rs). SRT declares a packet lost and retransmits it
        // once it is older than this window. If that window is shorter
        // than the shaper's queue, a packet that is merely waiting its
        // turn in the TBF gets retransmitted while the original is still
        // in flight — a false-loss storm that doubles offered load and
        // tips a busy bond into congestion collapse. 2s clears the 1s
        // buffer with margin.
        let out_uri = format!("srt://127.0.0.1:{SRTLA_SEND_SRT_PORT}?mode=caller&latency=2000");
        let mut caller = NamespaceProcess::spawn(
            &self.topo.sender_ns,
            "srt-live-transmit",
            &[&in_uri, &out_uri],
        )
        .context("start srt-live-transmit caller")?;

        std::thread::sleep(Duration::from_millis(750));
        if let Some((code, stderr)) = caller.check_exit() {
            bail!("srt caller exited immediately (code: {code:?})\nstderr:\n{stderr}");
        }
        wait_for_udp_listener(
            &self.topo.sender_ns,
            SRT_CALLER_INGEST_PORT,
            Duration::from_secs(5),
        )
        .context("wait for srt caller udp ingest")?;

        self.srt_caller = Some(caller);
        Ok(())
    }

    /// Start the adaptive SRT sender in the sender namespace, in front of
    /// srtla_send. The counterpart to [`start_srt_caller`] for tests that
    /// need a *realistic* offered load rather than a fixed one.
    ///
    /// `start_srt_caller` drives a constant bitrate, which oversubscribes
    /// a busy bond and collapses it into a retransmit storm. This instead
    /// runs a real SRT caller that lowers its rate when the SRT send
    /// buffer backs up — belacoder's congestion response without the
    /// encoder — so the offered rate tracks what the bond can carry and
    /// the run stays in a regime where per-link goodput is meaningful.
    ///
    /// Build the binary once with [`build_adaptive_sender`] and pass it
    /// in; the sender ramps between `min_kbps` and `max_kbps`.
    pub fn start_adaptive_sender(
        &mut self,
        sender_bin: &Path,
        min_kbps: u32,
        max_kbps: u32,
    ) -> Result<()> {
        let bin = sender_bin.to_string_lossy().into_owned();
        let port = SRTLA_SEND_SRT_PORT.to_string();
        let min_s = min_kbps.to_string();
        let max_s = max_kbps.to_string();
        // 2s SRT latency, above the shaper buffer — same reason as the
        // constant caller above.
        let mut sender = NamespaceProcess::spawn(
            &self.topo.sender_ns,
            &bin,
            &["127.0.0.1", &port, &min_s, &max_s, "2000"],
        )
        .context("start adaptive SRT sender")?;

        std::thread::sleep(Duration::from_millis(1500));
        if let Some((code, stderr)) = sender.check_exit() {
            bail!("adaptive sender exited immediately (code: {code:?})\nstderr:\n{stderr}");
        }

        // Reuse the caller slot: this *is* the SRT caller, and the slot's
        // lifecycle (kill on stop/drop) is exactly what we want.
        self.srt_caller = Some(sender);
        Ok(())
    }

    /// Query srtla_send's control socket for a `get_stats` snapshot,
    /// returning the parsed `result` object.
    ///
    /// Runs the query as root inside the namespace: srtla_send is spawned
    /// under sudo, so the socket it binds is root-owned and a test process
    /// running as the invoking user cannot connect to it directly.
    pub fn get_stats(&self, socket_path: &str) -> Result<serde_json::Value> {
        let script = format!(
            "import socket,sys\ns=socket.socket(socket.AF_UNIX,socket.SOCK_STREAM)\ns.\
             settimeout(5)\ns.connect('{socket_path}')\ns.sendall(b'{{\"jsonrpc\":\"2.0\",\"id\":\
             1,\"method\":\"get_stats\",\"params\":{{}}}}\\n')\nbuf=b''\nwhile not \
             buf.endswith(b'\\n'):\n\x20 c=s.recv(65536)\n\x20 if not c: break\n\x20 \
             buf+=c\ns.close()\nsys.stdout.write(buf.decode())"
        );
        let out = self
            .topo
            .sender_ns
            .exec_checked("python3", &["-c", &script])
            .context("query control socket")?;
        let raw = String::from_utf8_lossy(&out.stdout);
        let resp: serde_json::Value = serde_json::from_str(raw.trim())
            .with_context(|| format!("parse stats reply: {raw}"))?;
        resp.get("result")
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("no result in stats reply: {resp}"))
    }

    /// Apply impairment to sender-side link at `idx`.
    pub fn impair_link(&self, idx: usize, config: ImpairmentConfig) -> Result<()> {
        self.topo.impair_link(idx, config)
    }

    /// The local SRT port that srtla_send listens on (for injecting test data).
    pub fn sender_srt_port(&self) -> u16 {
        SRTLA_SEND_SRT_PORT
    }

    /// The receiver-side srtla_rec port the sender's uplink sockets connect to.
    pub fn receiver_srtla_port(&self) -> u16 {
        SRTLA_REC_PORT
    }

    /// Stop all processes and collect their output.
    pub fn stop(&mut self) -> StackOutput {
        let mut send_out = (vec![], vec![]);
        let mut rec_out = (vec![], vec![]);
        let mut srt_out = (vec![], vec![]);
        let mut caller_out = (vec![], vec![]);

        // Kill in reverse order: caller → sender → receiver → srt server
        if let Some(mut p) = self.srt_caller.take() {
            p.kill();
            caller_out = (p.stdout_lines(), p.stderr_lines());
        }
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
            srt_caller_stdout: caller_out.0,
            srt_caller_stderr: caller_out.1,
        }
    }
}

impl Drop for SrtlaTestStack {
    fn drop(&mut self) {
        // Ensure all processes are killed even if stop() wasn't called.
        // Dropping NamespaceProcess triggers its Drop impl which calls kill().
        drop(self.srt_caller.take());
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

/// Default datagram size: one MPEG-TS packet.
pub const TS_PACKET_BYTES: usize = 188;
/// libsrt's default payload size. Prefer this when a test needs real
/// throughput: a Python pump cannot reliably sleep in the sub-millisecond
/// intervals that megabit rates demand at 188 bytes a datagram, so it
/// silently becomes the bottleneck instead of the network.
pub const SRT_PAYLOAD_BYTES: usize = 1316;

/// Build the steady-rate UDP sender script shared by the blocking and
/// spawned stream injectors.
///
/// The loop paces against a wall-clock deadline per packet rather than
/// sleeping a fixed interval, so send-call overhead does not accumulate
/// into a drifting, ever-slower rate.
fn udp_stream_script(
    target_ip: &str,
    port: u16,
    packets_per_sec: u32,
    payload_bytes: usize,
    duration: Duration,
) -> Result<String> {
    if packets_per_sec == 0 {
        bail!("packets_per_sec must be > 0");
    }
    if payload_bytes == 0 {
        bail!("payload_bytes must be > 0");
    }
    let interval = 1.0 / f64::from(packets_per_sec);
    let dur_secs = duration.as_secs_f64();

    Ok(format!(
        "import socket,time\ns=socket.socket(socket.AF_INET,socket.SOCK_DGRAM)\nd=b'\\xb8'*\
         {payload_bytes}\nstart=time.time(); i=0\nwhile True:\n\x20   now=time.time()\n\x20   if \
         now-start>={dur_secs}: break\n\x20   s.sendto(d,('{target_ip}',{port}))\n\x20   \
         i+=1\n\x20   nxt=start+i*{interval}\n\x20   slp=nxt-time.time()\n\x20   if slp>0: \
         time.sleep(slp)\ns.close()\nprint(f'sent {{i}} packets')"
    ))
}

/// Inject UDP packets at a steady rate (packets/sec) for `duration`.
/// Blocks until the stream finishes.
pub fn inject_udp_stream(
    ns: &Namespace,
    target_ip: &str,
    port: u16,
    packets_per_sec: u32,
    payload_bytes: usize,
    duration: Duration,
) -> Result<()> {
    let script = udp_stream_script(target_ip, port, packets_per_sec, payload_bytes, duration)?;
    ns.exec_checked("python3", &["-c", &script])
        .context("inject UDP stream")?;
    Ok(())
}

/// Like [`inject_udp_stream`], but returns immediately with the running
/// process so the caller can observe the system *while* traffic flows.
/// Anything that samples a control loop's behaviour under load needs
/// this rather than the blocking form.
pub fn spawn_udp_stream(
    ns: &Namespace,
    target_ip: &str,
    port: u16,
    packets_per_sec: u32,
    payload_bytes: usize,
    duration: Duration,
) -> Result<NamespaceProcess> {
    let script = udp_stream_script(target_ip, port, packets_per_sec, payload_bytes, duration)?;
    NamespaceProcess::spawn(ns, "python3", &["-c", &script]).context("spawn UDP stream")
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
