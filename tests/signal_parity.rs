//! Signal & startup parity integration tests (Unix).
//!
//! These spawn the real `srtla_send` binary and drive it with POSIX signals to
//! pin the CeraLive signal/startup contract CeraUI depends on:
//!   * a SIGHUP with a garbage / empty file is refused and the stream stays up;
//!   * a missing ips file at startup yields an empty pool, not a crash;
//!   * a valid SIGHUP joins a new link without dropping the survivor;
//!   * SIGTERM / SIGINT exit cleanly (code 0) inside CeraUI's 10s SIGKILL
//!     window, and the telemetry stats file is unlinked.
//!
//! They complement the deterministic in-process unit tests in
//! `src/sender/{reload.rs, connections.rs}` and `src/tests/sender_tests.rs`.
#![cfg(unix)]

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_srtla_send");
const START_TIMEOUT: Duration = Duration::from_secs(5);
const LOG_TIMEOUT: Duration = Duration::from_secs(5);
const KILL_WINDOW: Duration = Duration::from_secs(10);
/// Clean shutdown must be *prompt* (the contract says "well within" CeraUI's 10s
/// SIGKILL window). T9 pins the tighter ~1s bound the device expects: we still
/// poll up to `KILL_WINDOW` so a slow exit is reported as a too-slow failure
/// rather than a flaky `None`.
const PROMPT_EXIT: Duration = Duration::from_millis(1500);

/// Grab an ephemeral UDP port for the local SRT listener, then release it so the
/// spawned binary can bind it (a small TOCTOU race is acceptable for a test).
fn free_udp_port() -> u16 {
    let sock = std::net::UdpSocket::bind("127.0.0.1:0").expect("bind ephemeral udp");
    sock.local_addr().unwrap().port()
}

/// Whether a second loopback source IP can be bound on this host. macOS only
/// configures 127.0.0.1 by default, so the multi-link reload test self-skips
/// there while still running for real on the Linux device/CI target.
fn second_loopback_bindable() -> bool {
    std::net::UdpSocket::bind("127.0.0.2:0").is_ok()
}

struct SenderProc {
    child: Child,
    logs: Arc<Mutex<String>>,
}

impl SenderProc {
    fn spawn(args: &[&str]) -> Self {
        let mut child = Command::new(BIN)
            .args(args)
            .env("RUST_LOG", "info")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn srtla_send");

        // tracing writes to stdout; panics/anyhow errors land on stderr. Pump both
        // into one buffer so log assertions see everything the binary emitted.
        let logs = Arc::new(Mutex::new(String::new()));
        let streams: [Box<dyn std::io::Read + Send>; 2] = [
            Box::new(child.stdout.take().expect("capture stdout")),
            Box::new(child.stderr.take().expect("capture stderr")),
        ];
        for stream in streams {
            let sink = logs.clone();
            thread::spawn(move || {
                for line in BufReader::new(stream).lines().map_while(Result::ok) {
                    let mut buf = sink.lock().unwrap();
                    buf.push_str(&line);
                    buf.push('\n');
                }
            });
        }

        Self { child, logs }
    }

    fn logs(&self) -> String {
        self.logs.lock().unwrap().clone()
    }

    fn wait_for_log(&self, needle: &str, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if self.logs().contains(needle) {
                return true;
            }
            thread::sleep(Duration::from_millis(50));
        }
        false
    }

    fn signal(&self, sig: &str) {
        let status = Command::new("kill")
            .args([format!("-{sig}"), self.child.id().to_string()])
            .status()
            .expect("run kill");
        assert!(status.success(), "kill -{sig} failed");
    }

    fn is_alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    fn wait_exit(&mut self, timeout: Duration) -> Option<i32> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            match self.child.try_wait() {
                Ok(Some(status)) => return Some(status.code().unwrap_or(-1)),
                Ok(None) => thread::sleep(Duration::from_millis(50)),
                Err(_) => return None,
            }
        }
        None
    }
}

impl Drop for SenderProc {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Spawn a sender against a one-line ips file in `dir`, returning the process and
/// the ips-file path so the test can rewrite it before signalling.
fn spawn_with_ips(
    dir: &std::path::Path,
    contents: &str,
    extra: &[&str],
) -> (SenderProc, std::path::PathBuf) {
    let ips = dir.join("ips.txt");
    std::fs::write(&ips, contents).expect("write ips file");
    let port = free_udp_port().to_string();
    let mut args = vec![
        port.as_str(),
        "127.0.0.1",
        "9999",
        ips.to_str().unwrap(),
        "--verbose",
    ];
    args.extend_from_slice(extra);
    (SenderProc::spawn(&args), ips)
}

#[test]
fn sighup_garbage_and_empty_files_are_refused_and_keep_the_stream_alive() {
    let dir = tempfile::tempdir().unwrap();
    let (mut proc, ips) = spawn_with_ips(dir.path(), "127.0.0.1\n", &[]);

    assert!(
        proc.wait_for_log("listening for SRT", START_TIMEOUT),
        "sender never started; logs:\n{}",
        proc.logs()
    );
    assert!(
        proc.wait_for_log("added uplink", START_TIMEOUT),
        "initial uplink never added; logs:\n{}",
        proc.logs()
    );

    // Pure garbage: reload must be refused with a specific parse error.
    std::fs::write(&ips, "this-is-not-an-ip\n###garbage###\n\n").unwrap();
    proc.signal("HUP");
    assert!(
        proc.wait_for_log("no valid source IPs", LOG_TIMEOUT),
        "garbage reload should be refused with a parse error; logs:\n{}",
        proc.logs()
    );
    assert!(
        !proc.logs().contains("stale connection"),
        "a refused reload must not tear any link down; logs:\n{}",
        proc.logs()
    );
    assert!(proc.is_alive(), "sender must survive a garbage reload");

    // Empty file: refused with the empty-specific message.
    std::fs::write(&ips, "\n   \n").unwrap();
    proc.signal("HUP");
    assert!(
        proc.wait_for_log("ips file is empty", LOG_TIMEOUT),
        "empty reload should be refused; logs:\n{}",
        proc.logs()
    );
    assert!(proc.is_alive(), "sender must survive an empty reload");
}

#[test]
fn missing_ips_file_at_startup_starts_empty_and_reloads_on_sighup() {
    let dir = tempfile::tempdir().unwrap();
    let ips = dir.path().join("ips.txt"); // intentionally not created
    let port = free_udp_port().to_string();
    let mut proc = SenderProc::spawn(&[
        port.as_str(),
        "127.0.0.1",
        "9999",
        ips.to_str().unwrap(),
        "--verbose",
    ]);

    assert!(
        proc.wait_for_log("empty uplink pool", START_TIMEOUT),
        "missing file should yield an empty pool; logs:\n{}",
        proc.logs()
    );
    assert!(
        proc.wait_for_log("listening for SRT", START_TIMEOUT),
        "sender must bind the listener even with no uplinks; logs:\n{}",
        proc.logs()
    );
    thread::sleep(Duration::from_secs(2));
    assert!(
        proc.is_alive(),
        "sender must not crash on a missing ips file"
    );

    // Writing the file and signalling must populate the pool.
    std::fs::write(&ips, "127.0.0.1\n").unwrap();
    proc.signal("HUP");
    assert!(
        proc.wait_for_log("added uplink", LOG_TIMEOUT),
        "SIGHUP after writing the file must add the uplink; logs:\n{}",
        proc.logs()
    );
    assert!(proc.is_alive());
}

#[test]
fn sighup_valid_reload_joins_new_link_without_dropping_survivor() {
    if !second_loopback_bindable() {
        eprintln!("Skipping: 127.0.0.2 is not locally bindable on this host");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let (mut proc, ips) = spawn_with_ips(dir.path(), "127.0.0.1\n", &[]);

    assert!(
        proc.wait_for_log("via 127.0.0.1", START_TIMEOUT),
        "initial uplink 127.0.0.1; logs:\n{}",
        proc.logs()
    );

    std::fs::write(&ips, "127.0.0.1\n127.0.0.2\n").unwrap();
    proc.signal("HUP");
    assert!(
        proc.wait_for_log("via 127.0.0.2", LOG_TIMEOUT),
        "the new link 127.0.0.2 must join on SIGHUP; logs:\n{}",
        proc.logs()
    );
    assert!(
        !proc.logs().contains("stale connection"),
        "the surviving link must not be torn down; logs:\n{}",
        proc.logs()
    );
    assert!(proc.is_alive());
}

#[test]
fn sigterm_exits_cleanly_and_unlinks_the_stats_file() {
    let dir = tempfile::tempdir().unwrap();
    let stats = dir.path().join("stats.json");
    // Pre-seed the stats path: shutdown must unlink whatever telemetry file is
    // there (the sink that writes it lands separately; the unlink is the
    // signal-parity half of the contract).
    std::fs::write(&stats, "{}").unwrap();
    let (mut proc, _ips) = spawn_with_ips(
        dir.path(),
        "127.0.0.1\n",
        &["--stats-file", stats.to_str().unwrap()],
    );

    assert!(
        proc.wait_for_log("listening for SRT", START_TIMEOUT),
        "sender never started; logs:\n{}",
        proc.logs()
    );

    let t0 = Instant::now();
    proc.signal("TERM");
    let code = proc.wait_exit(KILL_WINDOW);
    let elapsed = t0.elapsed();

    assert_eq!(code, Some(0), "SIGTERM must exit 0; logs:\n{}", proc.logs());
    assert!(
        elapsed < KILL_WINDOW,
        "must exit within the 10s SIGKILL window (took {elapsed:?})"
    );
    assert!(
        !stats.exists(),
        "the telemetry stats file must be unlinked on shutdown"
    );
}

#[test]
fn sigint_exits_cleanly_within_the_kill_window() {
    let dir = tempfile::tempdir().unwrap();
    let (mut proc, _ips) = spawn_with_ips(dir.path(), "127.0.0.1\n", &[]);

    assert!(
        proc.wait_for_log("listening for SRT", START_TIMEOUT),
        "sender never started; logs:\n{}",
        proc.logs()
    );

    let t0 = Instant::now();
    proc.signal("INT");
    let code = proc.wait_exit(KILL_WINDOW);

    assert_eq!(code, Some(0), "SIGINT must exit 0; logs:\n{}", proc.logs());
    assert!(
        t0.elapsed() < KILL_WINDOW,
        "must exit within the 10s window"
    );
}

#[test]
fn sigterm_unlinks_telemetry_exits_zero() {
    let dir = tempfile::tempdir().unwrap();
    let stats = dir.path().join("t9.json");
    let tmp = dir.path().join("t9.json.tmp");
    // Pre-seed both the live snapshot and a leftover temp sibling (as a crashed
    // atomic write would leave). Clean shutdown must unlink BOTH so no stale or
    // half-written telemetry outlives the process.
    std::fs::write(&stats, "{}").unwrap();
    std::fs::write(&tmp, "partial").unwrap();
    let (mut proc, _ips) = spawn_with_ips(
        dir.path(),
        "127.0.0.1\n",
        &["--stats-file", stats.to_str().unwrap()],
    );

    assert!(
        proc.wait_for_log("listening for SRT", START_TIMEOUT),
        "sender never started; logs:\n{}",
        proc.logs()
    );

    let t0 = Instant::now();
    proc.signal("TERM");
    let code = proc.wait_exit(KILL_WINDOW);
    let elapsed = t0.elapsed();

    assert_eq!(code, Some(0), "SIGTERM must exit 0; logs:\n{}", proc.logs());
    assert!(
        elapsed < PROMPT_EXIT,
        "SIGTERM must exit promptly (~1s); took {elapsed:?}"
    );
    assert!(
        !stats.exists(),
        "the live telemetry stats file must be unlinked on shutdown"
    );
    assert!(
        !tmp.exists(),
        "the telemetry .tmp sibling must be unlinked on shutdown"
    );
}

#[test]
fn empty_ip_file_at_startup_not_fatal() {
    let dir = tempfile::tempdir().unwrap();
    // The file exists but resolves to zero valid IPs (blank lines only) — the
    // empty-content sibling of the missing-file case covered above.
    let (mut proc, _ips) = spawn_with_ips(dir.path(), "\n   \n\n", &[]);

    assert!(
        proc.wait_for_log("empty uplink pool", START_TIMEOUT),
        "an empty ips file should yield an empty pool; logs:\n{}",
        proc.logs()
    );
    assert!(
        proc.wait_for_log("listening for SRT", START_TIMEOUT),
        "sender must bind the listener with no uplinks; logs:\n{}",
        proc.logs()
    );

    thread::sleep(Duration::from_secs(1));
    assert!(
        proc.is_alive(),
        "empty-start must keep running (no crash-loop); logs:\n{}",
        proc.logs()
    );

    proc.signal("TERM");
    assert_eq!(
        proc.wait_exit(KILL_WINDOW),
        Some(0),
        "SIGTERM after an empty start must still exit 0; logs:\n{}",
        proc.logs()
    );
}
