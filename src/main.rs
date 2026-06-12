use std::net::{IpAddr, SocketAddr};
use std::str::FromStr;

use anyhow::{Context, Result, anyhow};
use clap::Parser;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

// Use mimalloc as the global allocator for the binary (non-Windows only)
#[cfg(not(windows))]
#[global_allocator]
static ALLOC: mimalloc::MiMalloc = mimalloc::MiMalloc;

mod config;
mod connection;
mod ewma;
mod kalman;
mod mode;
mod protocol;
mod registration;
mod sender;
mod stats;
mod telemetry_file;
mod utils;

// Test helpers for binary tests
#[cfg(any(test, feature = "test-internals"))]
mod test_helpers;

use mode::SchedulingMode;

/// Default telemetry write cadence in milliseconds (`--stats-file-interval`).
const DEFAULT_STATS_FILE_INTERVAL_MS: u64 = 1000;

#[derive(Parser, Debug)]
#[command(
    name = "srtla_send",
    author,
    version,
    disable_version_flag = true,
    about = "SRTLA sender CLI",
    override_usage = "srtla_send [OPTIONS] SRT_LISTEN_PORT SRTLA_HOST SRTLA_PORT BIND_IPS_FILE"
)]
struct Cli {
    /// Print the version and exit
    #[arg(short = 'v', long = "version", action = clap::ArgAction::SetTrue)]
    print_version: bool,

    /// Local UDP port to listen for SRT packets (from srt-live-transmit or SRT
    /// app)
    #[arg(required_unless_present = "print_version")]
    local_srt_port: Option<u16>,
    /// Receiver host (srtla_rec or SRT listener)
    #[arg(required_unless_present = "print_version")]
    receiver_host: Option<String>,
    /// Receiver UDP port to send SRTLA packets to
    #[arg(required_unless_present = "print_version")]
    receiver_port: Option<u16>,
    /// Path to file containing newline-separated local source IPs to use for
    /// uplinks
    #[arg(required_unless_present = "print_version")]
    ips_file: Option<String>,

    /// Enable verbose (debug-level) logging
    #[arg(long = "verbose")]
    verbose: bool,

    /// Validate the IP list and resolve the receiver, then exit without binding
    /// sockets (non-zero exit if the IP list is unusable)
    #[arg(long = "dry-run")]
    dry_run: bool,

    /// Write per-uplink telemetry JSON to this path (ADR-001 stats file).
    /// Opt-in: absent means no telemetry file is ever created.
    #[arg(long = "stats-file")]
    stats_file: Option<String>,

    /// Telemetry write cadence in milliseconds for `--stats-file` (default 1000).
    #[arg(long = "stats-file-interval", default_value_t = DEFAULT_STATS_FILE_INTERVAL_MS)]
    stats_file_interval: u64,

    /// Unix domain socket path for remote toggle control (e.g.,
    /// /tmp/srtla.sock)
    #[arg(long = "control-socket")]
    control_socket: Option<String>,

    /// Scheduling mode: classic, enhanced (default), rtt-threshold
    #[arg(long = "mode", value_enum, default_value = "enhanced")]
    mode: SchedulingMode,
    /// Disable quality scoring (enhanced/rtt-threshold only)
    #[arg(long = "no-quality")]
    no_quality: bool,
    /// Enable connection exploration (enhanced only)
    #[arg(long = "exploration")]
    exploration: bool,
    /// RTT delta threshold in ms (rtt-threshold only, links within min_rtt + delta are "fast")
    #[arg(long = "rtt-delta-ms", default_value = "30")]
    rtt_delta_ms: u32,
}

/// Result of a `--dry-run` resolution: the parsed source IPs, any invalid
/// lines that were skipped, and the receiver addresses the host resolved to.
#[derive(Debug)]
struct DryRunReport {
    source_ips: Vec<IpAddr>,
    invalid_lines: Vec<(usize, String)>,
    receiver_addrs: Vec<SocketAddr>,
}

/// Validate the run configuration without binding any sockets.
///
/// Reads and parses `ips_file`, then resolves `receiver_host:receiver_port`.
/// Returns an error with a specific, actionable message when the IP list is
/// unusable (missing/unreadable, empty, or zero valid IPs) or when the
/// receiver address cannot be resolved. On success no sockets are bound — the
/// caller is expected to print the report and exit 0.
async fn dry_run_resolve(
    ips_file: &str,
    receiver_host: &str,
    receiver_port: u16,
) -> Result<DryRunReport> {
    let text = std::fs::read_to_string(ips_file)
        .with_context(|| format!("ips file not found or unreadable: {ips_file}"))?;

    let mut source_ips = Vec::new();
    let mut invalid_lines = Vec::new();
    for (idx, line) in text.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        match IpAddr::from_str(trimmed) {
            Ok(ip) => source_ips.push(ip),
            Err(_) => invalid_lines.push((idx + 1, trimmed.to_string())),
        }
    }

    if source_ips.is_empty() {
        return match invalid_lines.first() {
            Some((line_no, content)) => Err(anyhow!(
                "no valid source IPs in {ips_file}: first invalid entry on line {line_no} \
                 ('{content}')"
            )),
            None => Err(anyhow!("no valid source IPs in {ips_file}: file is empty")),
        };
    }

    let receiver_addrs: Vec<SocketAddr> = tokio::net::lookup_host((receiver_host, receiver_port))
        .await
        .with_context(|| {
            format!("failed to resolve receiver address {receiver_host}:{receiver_port}")
        })?
        .collect();

    if receiver_addrs.is_empty() {
        return Err(anyhow!(
            "receiver host {receiver_host}:{receiver_port} resolved to no addresses"
        ));
    }

    Ok(DryRunReport {
        source_ips,
        invalid_lines,
        receiver_addrs,
    })
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    let args = Cli::parse();

    if args.print_version {
        let version = env!("CARGO_PKG_VERSION");
        let git_hash = env!("GIT_HASH");
        let git_branch = env!("GIT_BRANCH");
        let git_dirty = env!("GIT_DIRTY");

        println!(
            "{} ({}@{}{}) [{}]",
            version,
            git_branch,
            git_hash,
            git_dirty,
            env!("CARGO_PKG_NAME")
        );
        return Ok(());
    }

    // `--verbose` raises the log level to debug (parity with the C sender's
    // `--verbose`); otherwise honor RUST_LOG via the env filter.
    let env_filter = if args.verbose {
        EnvFilter::new("debug")
    } else {
        EnvFilter::from_default_env()
    };
    tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .with_target(false)
        .init();

    let local_srt_port = args.local_srt_port.expect("required");
    let receiver_host = args.receiver_host.as_deref().expect("required");
    let receiver_port = args.receiver_port.expect("required");
    let ips_file = args.ips_file.as_deref().expect("required");

    if args.dry_run {
        let report = dry_run_resolve(ips_file, receiver_host, receiver_port).await?;
        for (line_no, content) in &report.invalid_lines {
            warn!("ignoring invalid IP on line {line_no}: '{content}'");
        }
        println!("dry-run: configuration valid; no sockets bound");
        println!(
            "receiver {receiver_host}:{receiver_port} resolves to {} address(es):",
            report.receiver_addrs.len()
        );
        for addr in &report.receiver_addrs {
            println!("  {addr}");
        }
        println!("source uplink IPs ({}):", report.source_ips.len());
        for ip in &report.source_ips {
            println!("  {ip}");
        }
        return Ok(());
    }

    if let Some(stats_file) = args.stats_file.as_deref() {
        info!(
            "telemetry stats-file requested at {stats_file} (cadence {} ms)",
            args.stats_file_interval
        );
    }

    let config = config::DynamicConfig::from_cli(
        args.mode,
        args.no_quality,
        args.exploration,
        args.rtt_delta_ms,
    );

    // Create shared stats for telemetry export
    let shared_stats = stats::SharedStats::new();

    // Start config listener (stdin or Unix socket)
    config::spawn_config_listener(config.clone(), args.control_socket, shared_stats.clone());

    let telemetry = args.stats_file.as_deref().map(|path| {
        telemetry_file::TelemetryWriter::new(path.to_string(), args.stats_file_interval)
    });

    let outcome = sender::run_sender_with_config(
        local_srt_port,
        receiver_host,
        receiver_port,
        ips_file,
        config,
        shared_stats,
        telemetry,
    )
    .await;

    // On shutdown (clean signal exit or fatal error) remove the telemetry stats
    // file so a stale snapshot never outlives the process: CeraUI respawns
    // srtla_send on every network change and would read a leftover file as live.
    if let Some(stats_file) = args.stats_file.as_deref() {
        let _ = std::fs::remove_file(stats_file);
        let _ = std::fs::remove_file(format!("{stats_file}.tmp"));
    }

    outcome.context("srtla_send failed")
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use clap::error::ErrorKind;

    use super::*;

    // ---- CLI parsing: positional contract --------------------------------

    #[test]
    fn parses_positional_contract_in_order() {
        let cli =
            Cli::try_parse_from(["srtla_send", "5000", "127.0.0.1", "5001", "/tmp/srtla_ips"])
                .expect("positional contract should parse");
        assert_eq!(cli.local_srt_port, Some(5000));
        assert_eq!(cli.receiver_host.as_deref(), Some("127.0.0.1"));
        assert_eq!(cli.receiver_port, Some(5001));
        assert_eq!(cli.ips_file.as_deref(), Some("/tmp/srtla_ips"));
        // Defaults for the additive flags.
        assert!(!cli.verbose);
        assert!(!cli.dry_run);
        assert_eq!(cli.stats_file, None);
        assert_eq!(cli.stats_file_interval, DEFAULT_STATS_FILE_INTERVAL_MS);
    }

    #[test]
    fn parses_the_ceraui_arg_vector_verbatim() {
        // The exact arg vector buildSrtlaSendArgs emits for a verbose stream
        // (see CeraUI srtla-bindings-skew.test.ts): positionals then --verbose.
        let cli = Cli::try_parse_from([
            "srtla_send",
            "9000",
            "relay.example.com",
            "8890",
            "/tmp/srtla_ips",
            "--verbose",
        ])
        .expect("CeraUI arg vector should parse");
        assert_eq!(cli.local_srt_port, Some(9000));
        assert_eq!(cli.receiver_host.as_deref(), Some("relay.example.com"));
        assert_eq!(cli.receiver_port, Some(8890));
        assert_eq!(cli.ips_file.as_deref(), Some("/tmp/srtla_ips"));
        assert!(cli.verbose);
    }

    #[test]
    fn flags_may_precede_positionals() {
        // clap accepts options before positionals; the positional order is
        // still load-bearing and must resolve identically.
        let cli = Cli::try_parse_from([
            "srtla_send",
            "--verbose",
            "5000",
            "127.0.0.1",
            "5001",
            "/tmp/srtla_ips",
        ])
        .expect("flags before positionals should parse");
        assert!(cli.verbose);
        assert_eq!(cli.local_srt_port, Some(5000));
        assert_eq!(cli.ips_file.as_deref(), Some("/tmp/srtla_ips"));
    }

    // ---- CLI parsing: individual flags -----------------------------------

    #[test]
    fn parses_stats_file_flag() {
        let cli = Cli::try_parse_from([
            "srtla_send",
            "5000",
            "127.0.0.1",
            "5001",
            "/tmp/srtla_ips",
            "--stats-file",
            "/tmp/srtla-send-stats-5000.json",
        ])
        .expect("--stats-file should parse");
        assert_eq!(
            cli.stats_file.as_deref(),
            Some("/tmp/srtla-send-stats-5000.json")
        );
    }

    #[test]
    fn parses_stats_file_interval_flag() {
        let cli = Cli::try_parse_from([
            "srtla_send",
            "5000",
            "127.0.0.1",
            "5001",
            "/tmp/srtla_ips",
            "--stats-file-interval",
            "500",
        ])
        .expect("--stats-file-interval should parse");
        assert_eq!(cli.stats_file_interval, 500);
    }

    #[test]
    fn parses_dry_run_flag() {
        let cli = Cli::try_parse_from([
            "srtla_send",
            "5000",
            "127.0.0.1",
            "5001",
            "/tmp/srtla_ips",
            "--dry-run",
        ])
        .expect("--dry-run should parse");
        assert!(cli.dry_run);
    }

    #[test]
    fn upstream_scheduler_flags_still_parse() {
        // The upstream scheduler / control-socket flags remain intact (kept
        // working, just undocumented in user-facing docs).
        let cli = Cli::try_parse_from([
            "srtla_send",
            "5000",
            "127.0.0.1",
            "5001",
            "/tmp/srtla_ips",
            "--mode",
            "classic",
            "--no-quality",
            "--exploration",
            "--rtt-delta-ms",
            "50",
            "--control-socket",
            "/tmp/srtla.sock",
        ])
        .expect("upstream flags should still parse");
        assert_eq!(cli.mode, SchedulingMode::Classic);
        assert!(cli.no_quality);
        assert!(cli.exploration);
        assert_eq!(cli.rtt_delta_ms, 50);
        assert_eq!(cli.control_socket.as_deref(), Some("/tmp/srtla.sock"));
    }

    #[test]
    fn version_flag_short_circuits_required_positionals() {
        for flag in ["-v", "--version"] {
            let cli = Cli::try_parse_from(["srtla_send", flag])
                .unwrap_or_else(|e| panic!("{flag} should parse without positionals: {e}"));
            assert!(cli.print_version);
            assert_eq!(cli.local_srt_port, None);
        }
    }

    // ---- CLI parsing: error paths (usage + non-zero exit) ----------------

    #[test]
    fn no_arguments_is_a_missing_required_argument_error() {
        let err = Cli::try_parse_from(["srtla_send"]).expect_err("missing positionals must error");
        assert_eq!(err.kind(), ErrorKind::MissingRequiredArgument);
    }

    #[test]
    fn partial_positionals_is_an_error() {
        // Only listen_port + host given; receiver_port + ips_file missing.
        let err = Cli::try_parse_from(["srtla_send", "5000", "127.0.0.1"])
            .expect_err("partial positionals must error");
        assert_eq!(err.kind(), ErrorKind::MissingRequiredArgument);
    }

    #[test]
    fn non_numeric_port_is_an_error() {
        let err = Cli::try_parse_from(["srtla_send", "notaport", "127.0.0.1", "5001", "/tmp/ips"])
            .expect_err("non-numeric port must error");
        assert_eq!(err.kind(), ErrorKind::ValueValidation);
    }

    // ---- --dry-run resolution: happy path --------------------------------

    fn write_temp_ips(contents: &str) -> tempfile::NamedTempFile {
        let mut f = tempfile::NamedTempFile::new().expect("create temp ips file");
        f.write_all(contents.as_bytes()).expect("write ips file");
        f.flush().expect("flush ips file");
        f
    }

    #[tokio::test]
    async fn dry_run_resolves_valid_ip_list() {
        let f = write_temp_ips("10.0.0.1\n10.0.1.2\n");
        let report = dry_run_resolve(f.path().to_str().unwrap(), "127.0.0.1", 5001)
            .await
            .expect("valid ip list should resolve");
        assert_eq!(
            report.source_ips,
            vec![
                IpAddr::from_str("10.0.0.1").unwrap(),
                IpAddr::from_str("10.0.1.2").unwrap(),
            ]
        );
        assert!(report.invalid_lines.is_empty());
        // 127.0.0.1:5001 must resolve to at least one socket address.
        assert!(!report.receiver_addrs.is_empty());
        assert!(report.receiver_addrs.iter().all(|a| a.port() == 5001));
    }

    #[tokio::test]
    async fn dry_run_skips_blank_and_invalid_lines_but_keeps_valid_ips() {
        let f = write_temp_ips("10.0.0.1\n\n  \nnot-an-ip\n10.0.2.3\n");
        let report = dry_run_resolve(f.path().to_str().unwrap(), "127.0.0.1", 5001)
            .await
            .expect("a list with one valid IP should still resolve");
        assert_eq!(
            report.source_ips,
            vec![
                IpAddr::from_str("10.0.0.1").unwrap(),
                IpAddr::from_str("10.0.2.3").unwrap(),
            ]
        );
        // The garbage line ("not-an-ip" on line 4) is reported but non-fatal.
        assert_eq!(report.invalid_lines, vec![(4, "not-an-ip".to_string())]);
    }

    // ---- --dry-run resolution: error paths -------------------------------

    #[tokio::test]
    async fn dry_run_missing_file_errors_with_specific_message() {
        let missing = "/tmp/srtla-dry-run-definitely-missing-file.txt";
        let err = dry_run_resolve(missing, "127.0.0.1", 5001)
            .await
            .expect_err("missing file must error");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("not found or unreadable") && msg.contains(missing),
            "unexpected error message: {msg}"
        );
    }

    #[tokio::test]
    async fn dry_run_empty_file_errors() {
        let f = write_temp_ips("\n   \n\n");
        let err = dry_run_resolve(f.path().to_str().unwrap(), "127.0.0.1", 5001)
            .await
            .expect_err("empty file must error");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("no valid source IPs") && msg.contains("empty"),
            "unexpected error message: {msg}"
        );
    }

    #[tokio::test]
    async fn dry_run_all_garbage_file_errors_with_line_number() {
        let f = write_temp_ips("garbage\nstill-not-an-ip\n");
        let err = dry_run_resolve(f.path().to_str().unwrap(), "127.0.0.1", 5001)
            .await
            .expect_err("all-garbage file must error");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("no valid source IPs") && msg.contains("line 1"),
            "unexpected error message: {msg}"
        );
    }
}
