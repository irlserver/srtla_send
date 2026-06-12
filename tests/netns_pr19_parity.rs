//! PR #19 sender-behavior parity verification under network namespaces (opt-in).
//!
//! Two real-traffic measurements against a live `srtla_rec`, proving the Rust
//! sender reproduces the *outcomes* the C PR #19 sender targets:
//!
//!   1. Per-connection keepalive cadence ~1 s (NAT-keepalive + telemetry liveness).
//!   2. Jitter demotion: a link that becomes jittery loses selection share but is
//!      never disconnected (matching `jitter-stress.sh`'s "down-weight, never reap").
//!
//! Both are evidence-level checks (packet capture grouped per source IP = the
//! selected uplink). They skip cleanly unless root/sudo + srtla_rec +
//! srt-live-transmit + tcpdump + python3 + netem are available, exactly like the
//! other `netns_*` suites, so the default `cargo test` gate stays green in CI.

mod common;

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};
use std::thread::sleep;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use network_sim::{
    NamespaceProcess, SrtlaTestTopology, check_binary, check_privileges, wait_for_udp_listener,
};

/// Both tests build a netns topology with pid-derived veth names, so they must
/// not run concurrently within this binary.
static SERIAL: Mutex<()> = Mutex::new(());
static CAP_SEQ: AtomicU32 = AtomicU32::new(0);

const LOCAL_SRT_PORT: u16 = 5560;
const SRTLA_PORT: u16 = 5000;
const SRT_PORT: u16 = 4001;
const SINK_URI: &str = "udp://127.0.0.1:9999";

fn deps_ok() -> bool {
    if !check_privileges() {
        eprintln!("Skipping netns_pr19_parity: requires root / passwordless sudo");
        return false;
    }
    for bin in ["srtla_rec", "srt-live-transmit", "tcpdump", "python3"] {
        if check_binary(bin).is_none() {
            eprintln!("Skipping netns_pr19_parity: '{bin}' not found in PATH");
            return false;
        }
    }
    let netem_ok = Command::new("sudo")
        .args(["modprobe", "sch_netem"])
        .output()
        .is_ok_and(|o| o.status.success());
    if !netem_ok {
        eprintln!("Skipping netns_pr19_parity: sch_netem unavailable");
        return false;
    }
    true
}

fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs_f64()
}

/// The full sender→receiver pipeline plus the per-link routing the bonding
/// topology needs (see `routing` notes below).
struct Stack {
    topo: SrtlaTestTopology,
    _srt: NamespaceProcess,
    _rec: NamespaceProcess,
    send: NamespaceProcess,
}

fn start_stack(name: &str) -> Stack {
    common::build_srtla_send();
    let topo = SrtlaTestTopology::new(name, 2).expect("create topology");

    let recv_ip = topo.receiver_ip.clone();
    let src1 = topo.sender_ips[1].clone();
    let sif1 = topo.sender_ifaces[1].clone();
    let rif1 = topo.receiver_ifaces[1].clone();

    // Loose reverse-path: the second link's reply path is asymmetric by design.
    for iface in ["all", "default"]
        .into_iter()
        .chain(topo.sender_ifaces.iter().map(String::as_str))
    {
        let _ = topo.sender_ns.exec(
            "sysctl",
            &["-w", &format!("net.ipv4.conf.{iface}.rp_filter=0")],
        );
    }
    for iface in ["all", "default"]
        .into_iter()
        .chain(topo.receiver_ifaces.iter().map(String::as_str))
    {
        let _ = topo.receiver_ns.exec(
            "sysctl",
            &["-w", &format!("net.ipv4.conf.{iface}.rp_filter=0")],
        );
    }

    // routing: without this only link 0 reaches the receiver, because both bound
    // source IPs resolve the receiver via link 0's subnet. Force link 1's source
    // to egress its own veth, and make the receiver answer link 1 *from* the IP
    // the sender connect()-ed to, so the connected uplink socket accepts the reply.
    let _ = topo.sender_ns.exec(
        "ip",
        &["route", "add", &recv_ip, "dev", &sif1, "table", "102"],
    );
    let _ = topo
        .sender_ns
        .exec("ip", &["rule", "add", "from", &src1, "lookup", "102"]);
    let _ = topo.receiver_ns.exec(
        "ip",
        &["route", "add", &src1, "dev", &rif1, "src", &recv_ip],
    );

    let srt_uri = format!("srt://:{SRT_PORT}?mode=listener");
    let srt = NamespaceProcess::spawn(
        &topo.receiver_ns,
        "srt-live-transmit",
        &[&srt_uri, SINK_URI],
    )
    .expect("start srt-live-transmit");
    sleep(Duration::from_millis(500));
    wait_for_udp_listener(&topo.receiver_ns, SRT_PORT, Duration::from_secs(5))
        .expect("srt-live-transmit listener");

    let srtla_port = SRTLA_PORT.to_string();
    let srt_port = SRT_PORT.to_string();
    let rec = NamespaceProcess::spawn(
        &topo.receiver_ns,
        "srtla_rec",
        &[
            "--srtla_port",
            &srtla_port,
            "--srt_hostname",
            "127.0.0.1",
            "--srt_port",
            &srt_port,
        ],
    )
    .expect("start srtla_rec");
    wait_for_udp_listener(&topo.receiver_ns, SRTLA_PORT, Duration::from_secs(5))
        .expect("srtla_rec listener");

    let ips = topo.write_ip_list().expect("write ip list");
    let bin = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/debug/srtla_send");
    let local = LOCAL_SRT_PORT.to_string();
    // RUST_LOG=info (not debug): heavy debug logging slows the 1 s housekeeping
    // tick enough to skew the keepalive cadence measurement.
    let send = NamespaceProcess::spawn_with_env(
        &topo.sender_ns,
        bin.to_str().expect("bin path"),
        &[
            local.as_str(),
            recv_ip.as_str(),
            srtla_port.as_str(),
            ips.to_str().expect("ips path"),
        ],
        &[("RUST_LOG", "info")],
    )
    .expect("start srtla_send");

    Stack {
        topo,
        _srt: srt,
        _rec: rec,
        send,
    }
}

/// Kill every process inside a namespace. `NamespaceProcess::kill` signals a
/// process group that the sudo-wrapped children are not leaders of, so it can
/// block forever on `wait`; killing the netns PIDs directly makes the wrappers
/// exit and keeps teardown bounded.
fn kill_netns_pids(ns_name: &str) {
    let Ok(out) = Command::new("sudo")
        .args(["ip", "netns", "pids", ns_name])
        .output()
    else {
        return;
    };
    let pids: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .map(String::from)
        .collect();
    if !pids.is_empty() {
        let mut args = vec!["kill".to_string(), "-9".to_string()];
        args.extend(pids);
        let _ = Command::new("sudo").args(&args).output();
    }
}

impl Drop for Stack {
    fn drop(&mut self) {
        kill_netns_pids(&self.topo.sender_ns.name);
        kill_netns_pids(&self.topo.receiver_ns.name);
    }
}

/// A `tcpdump` writing newline-per-packet `-tt -n` records to a temp file.
/// A file (not a pipe) avoids the 64 KiB pipe stall on data-rate captures.
struct Capture {
    child: Child,
    path: PathBuf,
    iface: String,
}

fn start_capture(ns_name: &str, iface: &str, filter: &str) -> Capture {
    let seq = CAP_SEQ.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("pr19cap_{}_{seq}.txt", std::process::id()));
    let file = std::fs::File::create(&path).expect("create capture file");
    let child = Command::new("sudo")
        .args([
            "ip", "netns", "exec", ns_name, "tcpdump", "-i", iface, "-tt", "-n", filter,
        ])
        .stdout(Stdio::from(file))
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn tcpdump");
    sleep(Duration::from_millis(300));
    Capture {
        child,
        path,
        iface: iface.to_string(),
    }
}

fn stop_capture(mut cap: Capture) -> Vec<f64> {
    let _ = Command::new("sudo")
        .args([
            "pkill",
            "-TERM",
            "-f",
            &format!("tcpdump -i {} ", cap.iface),
        ])
        .output();
    sleep(Duration::from_millis(400));
    let _ = cap.child.wait();
    let text = std::fs::read_to_string(&cap.path).unwrap_or_default();
    let _ = std::fs::remove_file(&cap.path);
    text.lines()
        .filter_map(|l| l.split_whitespace().next()?.parse::<f64>().ok())
        .collect()
}

fn spawn_injector(ns_name: &str, secs: f64) -> Child {
    let script = format!(
        "import socket,struct,time  # \
         PR19INJ\ns=socket.socket(socket.AF_INET,socket.SOCK_DGRAM)\npay=bytes(1316); seq=0; \
         t0=time.time()\nwhile time.time()-t0<{secs}: \
         s.sendto(struct.pack('>I',seq&0x7fffffff)+pay[4:],('127.0.0.1',{LOCAL_SRT_PORT})); \
         seq+=1; time.sleep(1/600)\ns.close()"
    );
    Command::new("sudo")
        .args(["ip", "netns", "exec", ns_name, "python3", "-c", &script])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn injector")
}

fn stop_injector(mut child: Child) {
    let _ = Command::new("sudo")
        .args(["pkill", "-TERM", "-f", "PR19INJ"])
        .output();
    let _ = child.wait();
}

fn count_in(ts: &[f64], lo: f64, hi: f64) -> usize {
    ts.iter().filter(|&&t| t >= lo && t <= hi).count()
}

fn median(mut v: Vec<f64>) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[v.len() / 2]
}

/// Merge keepalives sent <`gap` s apart into one round. The sender emits a
/// keepalive and, on RTT-measurement ticks, an immediate second probe; both are
/// one NAT-keepalive "round" for cadence purposes.
fn collapse_rounds(mut ts: Vec<f64>, gap: f64) -> Vec<f64> {
    ts.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let mut rounds: Vec<f64> = Vec::new();
    for t in ts {
        if rounds.last().is_none_or(|&last| t - last > gap) {
            rounds.push(t);
        }
    }
    rounds
}

/// Warm-up + readiness gate: returns once BOTH uplinks carry forwarded data, so
/// measurement never starts before link 1 finishes its registration handshake.
fn wait_links_active(stack: &Stack) -> bool {
    let if0 = stack.topo.sender_ifaces[0].clone();
    let if1 = stack.topo.sender_ifaces[1].clone();
    let ns = stack.topo.sender_ns.name.clone();
    let data = format!("udp and dst port {SRTLA_PORT} and udp[8] < 0x80");
    for _ in 0..12 {
        let c0 = start_capture(&ns, &if0, &data);
        let c1 = start_capture(&ns, &if1, &data);
        let inj = spawn_injector(&ns, 1.6);
        sleep(Duration::from_secs(2));
        stop_injector(inj);
        let n0 = stop_capture(c0).len();
        let n1 = stop_capture(c1).len();
        if n0 > 5 && n1 > 5 {
            return true;
        }
    }
    false
}

#[test]
fn keepalive_cadence_approx_1s_per_conn() {
    let _guard = SERIAL.lock().unwrap();
    if !deps_ok() {
        return;
    }
    let stack = start_stack("pr19ka");
    assert!(
        wait_links_active(&stack),
        "both uplinks failed to become active before cadence measurement"
    );

    let ns = stack.topo.sender_ns.name.clone();
    let if0 = stack.topo.sender_ifaces[0].clone();
    let if1 = stack.topo.sender_ifaces[1].clone();
    let ka = format!("udp and dst port {SRTLA_PORT} and udp[8:2] = 0x9000");

    let window = 25.0;
    let inj = spawn_injector(&ns, window + 3.0);
    let cap0 = start_capture(&ns, &if0, &ka);
    let cap1 = start_capture(&ns, &if1, &ka);
    sleep(Duration::from_secs_f64(window));
    let ka0 = stop_capture(cap0);
    let ka1 = stop_capture(cap1);
    stop_injector(inj);

    for (label, ts) in [("link0", ka0), ("link1", ka1)] {
        let n = ts.len();
        let rounds = collapse_rounds(ts.clone(), 0.3);
        let round_deltas: Vec<f64> = rounds.windows(2).map(|w| w[1] - w[0]).collect();
        let raw_deltas: Vec<f64> = {
            let mut s = ts.clone();
            s.sort_by(|a, b| a.partial_cmp(b).unwrap());
            s.windows(2).map(|w| w[1] - w[0]).collect()
        };
        let collapsed_median = median(round_deltas.clone());
        let round_rate = rounds.len() as f64 / window;
        let max_gap = round_deltas.iter().copied().fold(0.0_f64, |m, d| m.max(d));
        println!(
            "[cadence] {label}: keepalives={n} rounds={} round_rate={round_rate:.2}/s \
             raw_median={:.3}s collapsed_median={collapsed_median:.3}s max_gap={max_gap:.3}s",
            rounds.len(),
            median(raw_deltas),
        );

        // The sender targets 1 s (IDLE_TIME) but, like the C sender's whole-second
        // keepalive_due, the integer-second floor against the 1 s housekeeping tick
        // makes the effective cadence oscillate between ~1 s and ~2 s. What matters
        // for NAT/telemetry liveness is that it stays well under CONN_TIMEOUT (5 s).
        assert!(
            n >= 8,
            "{label}: too few keepalives captured ({n}) — link likely not established"
        );
        assert!(
            (0.4..=1.5).contains(&round_rate),
            "{label}: keepalive round rate {round_rate:.2}/s outside [0.4,1.5] (target ~1/s)"
        );
        assert!(
            (0.6..=2.2).contains(&collapsed_median),
            "{label}: keepalive cadence {collapsed_median:.3}s outside [0.6,2.2] (target ~1-2s)"
        );
        assert!(
            max_gap < 3.5,
            "{label}: max keepalive gap {max_gap:.3}s too close to CONN_TIMEOUT (5s)"
        );
    }
}

#[test]
fn jitter_demotes_link_selection_share() {
    let _guard = SERIAL.lock().unwrap();
    if !deps_ok() {
        return;
    }
    let mut stack = start_stack("pr19jit");
    assert!(
        wait_links_active(&stack),
        "both uplinks failed to become active before demotion measurement"
    );

    let ns = stack.topo.sender_ns.name.clone();
    let if0 = stack.topo.sender_ifaces[0].clone();
    let if1 = stack.topo.sender_ifaces[1].clone();
    let data = format!("udp and dst port {SRTLA_PORT} and udp[8] < 0x80");

    let inj = spawn_injector(&ns, 26.0);
    let cap0 = start_capture(&ns, &if0, &data);
    let cap1 = start_capture(&ns, &if1, &data);

    sleep(Duration::from_secs(2));
    let base_start = now();
    sleep(Duration::from_secs(10));
    let base_end = now();

    // Sustained jitter on link 1's egress (matches jitter-stress.sh phase 3).
    stack
        .topo
        .sender_ns
        .exec(
            "tc",
            &[
                "qdisc",
                "add",
                "dev",
                &if1,
                "root",
                "netem",
                "delay",
                "150ms",
                "200ms",
                "distribution",
                "normal",
            ],
        )
        .expect("apply jitter to link 1");
    let jit_start = now();
    sleep(Duration::from_secs(10));
    let jit_end = now();

    let t0 = stop_capture(cap0);
    let t1 = stop_capture(cap1);
    stop_injector(inj);

    let b0 = count_in(&t0, base_start, base_end);
    let b1 = count_in(&t1, base_start, base_end);
    let j0 = count_in(&t0, jit_start, jit_end);
    let j1 = count_in(&t1, jit_start, jit_end);

    let base_total = (b0 + b1).max(1) as f64;
    let jit_total = (j0 + j1).max(1) as f64;
    let base_share1 = 100.0 * b1 as f64 / base_total;
    let jit_share1 = 100.0 * j1 as f64 / jit_total;
    let rel_drop = if base_share1 > 0.0 {
        100.0 * (base_share1 - jit_share1) / base_share1
    } else {
        0.0
    };

    // Disconnect proof: established-link reconnect/failure log lines (info level).
    // Kill srtla_send via its netns PID so its stderr pipe reaches EOF.
    kill_netns_pids(&ns);
    let stderr = stack.send.stderr_lines().join("\n");
    let reconnects = stderr
        .matches("attempting full socket reconnection")
        .count()
        + stderr.matches("failed to reconnect").count()
        + stderr
            .matches("Failed to re-establish any connections")
            .count();

    println!("[demotion] baseline: link0={b0} link1={b1} (link1 {base_share1:.1}%)");
    println!("[demotion] jitter:   link0={j0} link1={j1} (link1 {jit_share1:.1}%)");
    println!(
        "[demotion] link1 relative share drop: {rel_drop:.1}% (need >=30) | established-link \
         reconnects: {reconnects}"
    );

    assert!(
        (25.0..=75.0).contains(&base_share1),
        "link1 baseline share {base_share1:.1}% not in [25,75]% — links not balanced before jitter"
    );
    assert!(
        rel_drop >= 30.0,
        "link1 share only dropped {rel_drop:.1}% under jitter (need >=30%): {base_share1:.1}% -> \
         {jit_share1:.1}%"
    );
    assert!(
        jit_share1 < base_share1,
        "link1 not demoted under jitter ({base_share1:.1}% -> {jit_share1:.1}%)"
    );
    assert!(
        j1 > 0,
        "link1 carried zero traffic under jitter — it was dropped, not demoted"
    );
    assert!(
        reconnects == 0,
        "established-link disconnects occurred under jitter ({reconnects})"
    );
    assert!(
        jit_total >= 0.4 * base_total,
        "bonded stream collapsed under jitter (jitter total {jit_total} vs baseline {base_total})"
    );
}
