use anyhow::{Context, Result};
use clap::Parser;
use tracing_subscriber::EnvFilter;

// Use mimalloc as the global allocator for the binary (non-Windows only)
#[cfg(not(windows))]
#[global_allocator]
static ALLOC: mimalloc::MiMalloc = mimalloc::MiMalloc;

mod config;
mod connection;
mod mode;
mod protocol;
mod registration;
mod sender;
mod stats;
mod utils;

// Test helpers for binary tests
#[cfg(any(test, feature = "test-internals"))]
mod test_helpers;

use mode::SchedulingMode;

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

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_target(false)
        .init();

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

    let local_srt_port = args.local_srt_port.expect("required");
    let receiver_host = args.receiver_host.as_deref().expect("required");
    let receiver_port = args.receiver_port.expect("required");
    let ips_file = args.ips_file.as_deref().expect("required");

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

    sender::run_sender_with_config(
        local_srt_port,
        receiver_host,
        receiver_port,
        ips_file,
        config,
        shared_stats,
    )
    .await
    .context("srtla_send failed")
}
