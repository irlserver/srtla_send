// The CLI is a thin shell over the `srtla_send` library: it parses arguments,
// starts the optional sidecars, picks an egress binder for the platform, and
// hands off to `sender::run_sender_with_config`. Consuming the library rather
// than re-declaring its modules keeps one compilation of the tree instead of
// two, and keeps the library's embedder-facing surface (the Android and Apple
// binders no CLI ever constructs) from reading as dead code here.
use anyhow::{Context, Result};
use clap::builder::{PossibleValuesParser, TypedValueParser};
use clap::parser::ValueSource;
use clap::{ArgMatches, CommandFactory, FromArgMatches, Parser};
use srtla_core::mode::SchedulingMode;
use srtla_send::{
    config, control_socket, metrics, net, priority_listener, sender, stats, subscriptions,
    toml_config, version,
};
use tracing_subscriber::EnvFilter;

// Use mimalloc as the global allocator for the binary (non-Windows only).
// Gated default-on via the `mimalloc` feature; --no-default-features builds
// fall back to the system allocator.
#[cfg(all(not(windows), feature = "mimalloc"))]
#[global_allocator]
static ALLOC: mimalloc::MiMalloc = mimalloc::MiMalloc;

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

    /// Path to a TOML config file, read once at startup. Keys match the long
    /// flag names with underscores (`conn_timeout_ms`); a flag given on the
    /// command line wins over the file
    #[arg(long = "config")]
    config_file: Option<String>,

    /// Scheduling mode: classic, enhanced (default)
    //
    // `SchedulingMode` is a pure core type and deliberately knows nothing about
    // clap. The CLI coupling lives here in the shell: parse the allowed strings
    // (which drive --help text and completion) and map through the core type's
    // own `FromStr`.
    #[arg(
        long = "mode",
        default_value = "enhanced",
        value_parser = PossibleValuesParser::new(["classic", "enhanced"])
            .map(|s| s.parse::<SchedulingMode>().expect("possible values are valid modes")),
    )]
    mode: SchedulingMode,
    /// Disable quality scoring (enhanced only)
    #[arg(long = "no-quality")]
    no_quality: bool,

    /// Disable the stalled-link deselect guard (on by default). The guard skips
    /// a link whose in-flight backlog is high while its last delivery proof has
    /// gone stale, provided a healthier link can carry the traffic. While
    /// gated the link carries keepalives plus a sparse duplicate-packet probe
    /// trickle; it rejoins after delivery proof has been sustained for the
    /// rejoin dwell (quick to drop, conservative to rejoin).
    #[arg(long = "no-stall-deselect")]
    no_stall_deselect: bool,
    /// In-flight packet backlog at or above which a link becomes a stall
    /// candidate for `--no-stall-deselect`.
    #[arg(long = "stall-min-in-flight", default_value_t = config::STALL_MIN_IN_FLIGHT_PACKETS)]
    stall_min_in_flight: i32,
    /// Ceiling (ms) on the delivery-proof staleness window after which a
    /// stall-candidate link is deselected. The effective window is
    /// RTT-adaptive (4x smoothed RTT, floored at 1000 ms) and this value caps
    /// it; links without an RTT baseline use the ceiling directly.
    #[arg(long = "stall-ack-stale-ms", default_value_t = config::STALL_ACK_STALE_MS)]
    stall_ack_stale_ms: u64,
    /// Per-link liveness timeout (ms): silence past this tears the link down
    /// and re-registers it. Also settable at runtime over JSON-RPC
    /// (`set_conn_timeout`) so a latency-aware client can scale it to
    /// `max(default, 2 x latency budget)` — an outage the receiver buffer
    /// can absorb should resume warm, not re-handshake. Clamped to
    /// 1000..=60000.
    #[arg(long = "conn-timeout-ms", default_value_t = config::CONN_TIMEOUT_MS)]
    conn_timeout_ms: u64,

    /// Disable whole-bond re-home (on by default). When every uplink has been
    /// down for longer than the all-links-failed window AND the receiver
    /// hostname no longer resolves to the address the bond is pinned to, the
    /// sender moves every uplink together to the newly-resolved address and
    /// re-registers from scratch over the ordinary REG1/REG2/REG3 flow. It
    /// never touches a bond with a live uplink, never follows a merely
    /// reordered DNS answer, and attempts at most one migration per minute.
    /// Pass this to keep the pre-existing behaviour of staying on the cached
    /// address until the process is restarted.
    #[arg(long = "no-rehome")]
    no_rehome: bool,

    /// UDP bind address for the keyframe priority sidecar. The encoder
    /// front-end sends 5-byte datagrams here to open a critical routing
    /// window. Unauthenticated same-device IPC: bind loopback. Omit to
    /// disable the sidecar (the keyframe-priority override is then inactive).
    /// Example: `127.0.0.1:7000`.
    #[arg(long = "priority-bind")]
    priority_bind: Option<std::net::SocketAddr>,

    /// TCP bind address for the Prometheus `/metrics` scrape endpoint.
    /// Unauthenticated: bind loopback. Omit to disable.
    /// Example: `127.0.0.1:9099`.
    #[arg(long = "metrics-bind")]
    metrics_bind: Option<std::net::SocketAddr>,
}

impl Cli {
    /// Replace each option the user did not type on the command line with its
    /// value from the config file. The precedence is command line, then file,
    /// then the clap default.
    fn apply_config_file(&mut self, matches: &ArgMatches, file: toml_config::TomlConfig) {
        let typed = |id: &str| matches.value_source(id) == Some(ValueSource::CommandLine);
        // The derive names each arg after its field, so one identifier serves
        // as both the arg id and the field to fill.
        macro_rules! fill {
            ($($field:ident),+ $(,)?) => {$(
                if let Some(value) = file.$field {
                    if !typed(stringify!($field)) {
                        self.$field = value;
                    }
                }
            )+};
        }
        fill!(
            mode,
            no_quality,
            no_stall_deselect,
            stall_min_in_flight,
            stall_ack_stale_ms,
            conn_timeout_ms,
        );
    }
}

/// Warn when a sidecar is bound to a non-loopback address. These endpoints
/// are unauthenticated same-device IPC (encoder front-end and local scrapers),
/// so a routable bind exposes an open control / scrape surface. We warn rather
/// than refuse so an operator can still bind elsewhere on a trusted network if
/// they explicitly choose to.
fn warn_if_not_loopback(what: &str, addr: std::net::SocketAddr) {
    if !addr.ip().is_loopback() {
        tracing::warn!(
            %addr,
            "{what} bound to a non-loopback address; it is unauthenticated and \
             should normally bind 127.0.0.1 / ::1"
        );
    }
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_target(false)
        .init();

    let matches = Cli::command().get_matches();
    let mut args = Cli::from_arg_matches(&matches).unwrap_or_else(|e| e.exit());
    if args.print_version {
        println!("{}", version::version_line());
        return Ok(());
    }

    if let Some(path) = args.config_file.clone() {
        let file = toml_config::TomlConfig::load(std::path::Path::new(&path))?;
        tracing::info!("loaded config from {path}");
        args.apply_config_file(&matches, file);
    }

    let local_srt_port = args.local_srt_port.expect("required");
    let receiver_host = args.receiver_host.as_deref().expect("required");
    let receiver_port = args.receiver_port.expect("required");
    let ips_file = args.ips_file.as_deref().expect("required");

    let config = config::DynamicConfig::from_cli(
        args.mode,
        args.no_quality,
        args.no_stall_deselect,
        args.stall_min_in_flight,
        args.stall_ack_stale_ms,
        args.conn_timeout_ms,
        args.no_rehome,
    );

    // Create shared stats for telemetry export
    let shared_stats = stats::SharedStats::new();

    let subscription_hub = subscriptions::SubscriptionHub::new();

    let critical_window = srtla_core::priority::CriticalWindow::new();
    if let Some(bind) = args.priority_bind {
        warn_if_not_loopback("priority sidecar (--priority-bind)", bind);
        priority_listener::spawn_listener(
            bind,
            critical_window.clone(),
            Some(subscription_hub.clone()),
        );
    }

    if let Some(bind) = args.metrics_bind {
        warn_if_not_loopback("metrics endpoint (--metrics-bind)", bind);
        metrics::spawn_server(
            bind,
            shared_stats.clone(),
            config.clone(),
            critical_window.clone(),
        );
    }

    // Stdin reader stays blocking; Unix socket goes async to support
    // subscription pushes.
    config::spawn_stdin_listener(
        config.clone(),
        shared_stats.clone(),
        critical_window.clone(),
    );
    if let Some(sock_path) = args.control_socket {
        control_socket::spawn(
            sock_path,
            config.clone(),
            shared_stats.clone(),
            critical_window.clone(),
            subscription_hub.clone(),
        );
    }

    // The CLI binds each uplink by its source IP, which on a multi-homed host
    // selects the egress via source-based routing. Darwin has no source-based
    // routing, so there the same IP list is resolved to interfaces and pinned
    // with IP_BOUND_IF instead; binding the source address alone would leave
    // every uplink on the default route.
    #[cfg(not(target_vendor = "apple"))]
    let binder: std::sync::Arc<dyn net::UplinkBinder> = std::sync::Arc::new(net::SourceIpBinder);
    #[cfg(target_vendor = "apple")]
    let binder: std::sync::Arc<dyn net::UplinkBinder> =
        std::sync::Arc::new(net::AppleInterfaceBinder::new());

    sender::run_sender_with_config(
        local_srt_port,
        receiver_host,
        receiver_port,
        ips_file,
        config,
        shared_stats,
        critical_window,
        subscription_hub,
        binder,
    )
    .await
    .context("srtla_send failed")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_with_file(flags: &[&str], file: &str) -> Cli {
        let argv = ["srtla_send", "5000", "rec.example", "5001", "ips.txt"]
            .into_iter()
            .chain(flags.iter().copied());
        let matches = Cli::command().try_get_matches_from(argv).unwrap();
        let mut cli = Cli::from_arg_matches(&matches).unwrap();
        cli.apply_config_file(&matches, toml::from_str(file).unwrap());
        cli
    }

    #[test]
    fn file_overrides_clap_defaults() {
        let cli = parse_with_file(
            &[],
            r#"
                mode = "classic"
                no_quality = true
                no_stall_deselect = true
                stall_min_in_flight = 64
                stall_ack_stale_ms = 1500
                conn_timeout_ms = 8000
            "#,
        );
        assert_eq!(cli.mode, SchedulingMode::Classic);
        assert!(cli.no_quality);
        assert!(cli.no_stall_deselect);
        assert_eq!(cli.stall_min_in_flight, 64);
        assert_eq!(cli.stall_ack_stale_ms, 1500);
        assert_eq!(cli.conn_timeout_ms, 8000);
    }

    #[test]
    fn typed_flag_overrides_file_even_at_default_value() {
        let default = config::CONN_TIMEOUT_MS.to_string();
        let cli = parse_with_file(
            &["--conn-timeout-ms", &default, "--no-quality"],
            "conn_timeout_ms = 8000\nno_quality = false",
        );
        assert_eq!(cli.conn_timeout_ms, config::CONN_TIMEOUT_MS);
        assert!(cli.no_quality);
    }

    #[test]
    fn absent_file_keys_keep_cli_values() {
        let cli = parse_with_file(&["--stall-ack-stale-ms", "2000"], "");
        assert_eq!(cli.mode, SchedulingMode::Enhanced);
        assert_eq!(cli.stall_ack_stale_ms, 2000);
        assert_eq!(cli.conn_timeout_ms, config::CONN_TIMEOUT_MS);
    }
}
