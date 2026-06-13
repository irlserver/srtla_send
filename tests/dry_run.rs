//! `--dry-run` exit-code & stderr contract tests (CLI level).
//!
//! These spawn the real `srtla_send` binary and pin the CeraLive parity
//! contract for the `--dry-run` flag (AGENTS.md PARITY CONTRACT):
//!   * a valid IP list + resolvable host prints the resolved receiver +
//!     uplink IPs and exits `0` **without binding any socket**;
//!   * an unusable IP list (missing/unreadable, empty, all-invalid) or an
//!     unresolvable host exits with a fixed non-zero code (`1`, anyhow's
//!     `ExitCode::FAILURE`) and a specific stderr message.
//!
//! They complement the in-process `dry_run_resolve` unit tests in
//! `src/main.rs` by exercising the actual process boundary CeraUI drives.

use std::io::Write;
use std::net::UdpSocket;
use std::process::{Command, Output};

use tempfile::NamedTempFile;

const BIN: &str = env!("CARGO_BIN_EXE_srtla_send");

/// Pinned failure exit code: `main` returns `anyhow::Result`, so any `Err`
/// becomes `ExitCode::FAILURE` (1). The contract is "fixed non-zero"; we pin
/// the exact value so a regression to some other code is caught.
const FAILURE_CODE: i32 = 1;

/// Bind an ephemeral UDP port and keep the socket so the spawned binary cannot
/// bind it. A successful dry-run against this occupied port proves the dry-run
/// path never touched the local SRT listener socket.
fn occupied_udp_port() -> (UdpSocket, u16) {
    let sock = UdpSocket::bind("127.0.0.1:0").expect("bind ephemeral udp");
    let port = sock.local_addr().unwrap().port();
    (sock, port)
}

fn write_temp_ips(contents: &str) -> NamedTempFile {
    let mut f = NamedTempFile::new().expect("create temp ips file");
    f.write_all(contents.as_bytes()).expect("write ips file");
    f.flush().expect("flush ips file");
    f
}

/// Spawn `srtla_send <listen_port> <host> <port> <ips_file> --dry-run` and
/// collect its full output. `RUST_LOG` is silenced so stdout carries only the
/// dry-run report lines.
fn run_dry_run(listen_port: u16, host: &str, recv_port: u16, ips_file: &str) -> Output {
    Command::new(BIN)
        .args([
            &listen_port.to_string(),
            host,
            &recv_port.to_string(),
            ips_file,
            "--dry-run",
        ])
        .env("RUST_LOG", "off")
        .output()
        .expect("spawn srtla_send")
}

// ---- success path: exit 0, report on stdout, no socket bound -------------

#[test]
fn dry_run_valid_exits_zero() {
    // Hold the listener port hostage: if dry-run bound it, the run would fail.
    let (_held, listen_port) = occupied_udp_port();
    let ips = write_temp_ips("127.0.0.1\n");

    let out = run_dry_run(listen_port, "127.0.0.1", 5000, ips.path().to_str().unwrap());

    assert_eq!(
        out.status.code(),
        Some(0),
        "valid dry-run must exit 0; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("no sockets bound"),
        "stdout must state no sockets bound; got: {stdout}"
    );
    // Resolved receiver address is listed.
    assert!(
        stdout.contains("receiver 127.0.0.1:5000 resolves to"),
        "stdout must list the resolved receiver; got: {stdout}"
    );
    assert!(
        stdout.contains("127.0.0.1:5000"),
        "stdout must contain the resolved receiver socket addr; got: {stdout}"
    );
    // Source uplink IP is listed.
    assert!(
        stdout.contains("source uplink IPs (1):"),
        "stdout must list the uplink IP count; got: {stdout}"
    );
}

#[test]
fn dry_run_does_not_bind_listener_socket() {
    // The occupied port is the strongest no-bind proof: the binary would error
    // with "address in use" if the dry-run path bound the listener.
    let (_held, listen_port) = occupied_udp_port();
    let ips = write_temp_ips("127.0.0.1\n");

    let out = run_dry_run(listen_port, "127.0.0.1", 5000, ips.path().to_str().unwrap());

    assert_eq!(
        out.status.code(),
        Some(0),
        "dry-run must exit 0 even when the listener port is already in use (it must not bind it); \
         stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

// ---- failure paths: fixed non-zero exit + specific stderr ----------------

#[test]
fn dry_run_empty_ip_file_exits_nonzero() {
    let (_held, listen_port) = occupied_udp_port();
    let ips = write_temp_ips("\n   \n\n");

    let out = run_dry_run(listen_port, "127.0.0.1", 5000, ips.path().to_str().unwrap());

    assert_eq!(
        out.status.code(),
        Some(FAILURE_CODE),
        "empty ip file must exit {FAILURE_CODE}"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("no valid source IPs") && stderr.contains("empty"),
        "stderr must name the empty-list cause; got: {stderr}"
    );
}

#[test]
fn dry_run_missing_ip_file_exits_nonzero() {
    let (_held, listen_port) = occupied_udp_port();
    let missing = "/tmp/srtla-send-rs-task7-definitely-missing.txt";

    let out = run_dry_run(listen_port, "127.0.0.1", 5000, missing);

    assert_eq!(
        out.status.code(),
        Some(FAILURE_CODE),
        "missing ip file must exit {FAILURE_CODE}"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("not found or unreadable") && stderr.contains(missing),
        "stderr must name the unreadable file; got: {stderr}"
    );
}

#[test]
fn dry_run_all_invalid_ip_file_exits_nonzero() {
    let (_held, listen_port) = occupied_udp_port();
    let ips = write_temp_ips("garbage\nstill-not-an-ip\n");

    let out = run_dry_run(listen_port, "127.0.0.1", 5000, ips.path().to_str().unwrap());

    assert_eq!(
        out.status.code(),
        Some(FAILURE_CODE),
        "all-invalid ip file must exit {FAILURE_CODE}"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("no valid source IPs") && stderr.contains("line 1"),
        "stderr must name the first invalid line; got: {stderr}"
    );
}

#[test]
fn dry_run_unresolvable_host_exits_nonzero() {
    let (_held, listen_port) = occupied_udp_port();
    // `.invalid` is reserved by RFC 6761 and must never resolve.
    let ips = write_temp_ips("127.0.0.1\n");

    let out = run_dry_run(
        listen_port,
        "srtla-send-rs-task7.invalid",
        5000,
        ips.path().to_str().unwrap(),
    );

    assert_eq!(
        out.status.code(),
        Some(FAILURE_CODE),
        "unresolvable host must exit {FAILURE_CODE}; stdout: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("failed to resolve receiver address"),
        "stderr must name the resolution failure; got: {stderr}"
    );
}
