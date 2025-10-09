use anyhow::{Context, Result};
use clap::Parser;
use tracing_subscriber::EnvFilter;

mod connection;
mod protocol;
mod registration;
mod sender;
mod toggles;
mod utils;

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

    /// Enable classic mode (disables all enhancements)
    #[arg(long = "classic")]
    classic: bool,
    /// Disable quality scoring
    #[arg(long = "no-quality")]
    no_quality: bool,
    /// Enable connection exploration
    #[arg(long = "exploration")]
    exploration: bool,
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

    let toggles =
        toggles::DynamicToggles::from_cli(args.classic, args.no_quality, args.exploration);

    // Start toggle listener (stdin or Unix socket)
    toggles::spawn_toggle_listener(toggles.clone(), args.control_socket);

    sender::run_sender_with_toggles(
        local_srt_port,
        receiver_host,
        receiver_port,
        ips_file,
        toggles,
    )
    .await
    .context("srtla_send failed")
}
