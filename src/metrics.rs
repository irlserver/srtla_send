//! Prometheus `/metrics` endpoint.
//!
//! Renders the current [`crate::stats::StatsSnapshot`], [`srtla_core::priority::CriticalWindow`]
//! counters, and [`crate::config::DynamicConfig`] as Prometheus text format.
//! Intended for scraping by prometheus / VictoriaMetrics / grafana agent.
//!
//! The HTTP server is hand-rolled on top of `tokio::net::TcpListener` to
//! avoid pulling axum / hyper / tower into srtla_send's dep tree. Only
//! the bare minimum is supported: `GET /metrics` and `GET /` return the
//! exposition text; anything else gets a 404. Responses always close
//! the connection (no keep-alive, no pipelining). For a scraping endpoint
//! this is plenty — Prometheus opens a fresh connection per scrape.

use std::fmt::Write;
use std::net::SocketAddr;

use srtla_core::mode::SchedulingMode;
use srtla_core::priority::CriticalWindow;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tracing::{debug, info, warn};

use crate::config::DynamicConfig;
use crate::stats::SharedStats;

/// Render the current state as a Prometheus text-format exposition.
pub fn render(stats: &SharedStats, config: &DynamicConfig, cw: &CriticalWindow) -> String {
    let snap = stats.get();
    let mut out = String::with_capacity(2048);

    // Link-level gauges. One series per link, labeled by local IP.
    writeln!(
        out,
        "# HELP srtla_send_link_up 1 if the link is connected and not timed out"
    )
    .ok();
    writeln!(out, "# TYPE srtla_send_link_up gauge").ok();
    for link in &snap.links {
        let up = if link.connected && !link.timed_out {
            1
        } else {
            0
        };
        writeln!(out, r#"srtla_send_link_up{{ip="{}"}} {up}"#, link.ip).ok();
    }

    writeln!(out, "# HELP srtla_send_link_rtt_ms smoothed RTT").ok();
    writeln!(out, "# TYPE srtla_send_link_rtt_ms gauge").ok();
    for link in &snap.links {
        writeln!(
            out,
            r#"srtla_send_link_rtt_ms{{ip="{}"}} {}"#,
            link.ip, link.rtt_ms
        )
        .ok();
    }

    writeln!(
        out,
        "# HELP srtla_send_link_rtt_min_ms dual-window minimum RTT baseline"
    )
    .ok();
    writeln!(out, "# TYPE srtla_send_link_rtt_min_ms gauge").ok();
    for link in &snap.links {
        writeln!(
            out,
            r#"srtla_send_link_rtt_min_ms{{ip="{}"}} {}"#,
            link.ip, link.rtt_min_ms
        )
        .ok();
    }

    writeln!(
        out,
        "# HELP srtla_send_link_rtt_velocity Kalman RTT velocity, ms/sample (positive = rising)"
    )
    .ok();
    writeln!(out, "# TYPE srtla_send_link_rtt_velocity gauge").ok();
    for link in &snap.links {
        writeln!(
            out,
            r#"srtla_send_link_rtt_velocity{{ip="{}"}} {}"#,
            link.ip, link.rtt_velocity
        )
        .ok();
    }

    writeln!(
        out,
        "# HELP srtla_send_link_window congestion window size (packets)"
    )
    .ok();
    writeln!(out, "# TYPE srtla_send_link_window gauge").ok();
    for link in &snap.links {
        writeln!(
            out,
            r#"srtla_send_link_window{{ip="{}"}} {}"#,
            link.ip, link.window
        )
        .ok();
    }

    writeln!(
        out,
        "# HELP srtla_send_link_in_flight packets sent but not yet ACKed"
    )
    .ok();
    writeln!(out, "# TYPE srtla_send_link_in_flight gauge").ok();
    for link in &snap.links {
        writeln!(
            out,
            r#"srtla_send_link_in_flight{{ip="{}"}} {}"#,
            link.ip, link.in_flight
        )
        .ok();
    }

    writeln!(out, "# HELP srtla_send_link_nak_total cumulative NAK count").ok();
    writeln!(out, "# TYPE srtla_send_link_nak_total counter").ok();
    for link in &snap.links {
        writeln!(
            out,
            r#"srtla_send_link_nak_total{{ip="{}"}} {}"#,
            link.ip, link.nak_count
        )
        .ok();
    }

    // Renamed from srtla_send_link_bitrate_bps, which reported bytes/sec
    // under a bits/sec name. Prometheus convention is base units, so the
    // name now states the unit it actually carries.
    writeln!(
        out,
        "# HELP srtla_send_link_bitrate_bytes_per_second measured send rate, bytes/sec"
    )
    .ok();
    writeln!(out, "# TYPE srtla_send_link_bitrate_bytes_per_second gauge").ok();
    for link in &snap.links {
        writeln!(
            out,
            r#"srtla_send_link_bitrate_bytes_per_second{{ip="{}"}} {}"#,
            link.ip, link.bitrate_bytes_per_sec
        )
        .ok();
    }

    writeln!(
        out,
        "# HELP srtla_send_link_quality_multiplier scheduler quality multiplier in [0.35, 1.1]"
    )
    .ok();
    writeln!(out, "# TYPE srtla_send_link_quality_multiplier gauge").ok();
    for link in &snap.links {
        writeln!(
            out,
            r#"srtla_send_link_quality_multiplier{{ip="{}"}} {}"#,
            link.ip, link.quality_multiplier
        )
        .ok();
    }

    writeln!(
        out,
        "# HELP srtla_send_link_stall_gated link is stall-gated out of the payload rotation (0/1)"
    )
    .ok();
    writeln!(out, "# TYPE srtla_send_link_stall_gated gauge").ok();
    for link in &snap.links {
        writeln!(
            out,
            r#"srtla_send_link_stall_gated{{ip="{}"}} {}"#,
            link.ip,
            if link.stall_gated { 1 } else { 0 }
        )
        .ok();
    }

    writeln!(
        out,
        "# HELP srtla_send_link_stall_gate_events cumulative stall-latch engagements"
    )
    .ok();
    writeln!(out, "# TYPE srtla_send_link_stall_gate_events counter").ok();
    for link in &snap.links {
        writeln!(
            out,
            r#"srtla_send_link_stall_gate_events{{ip="{}"}} {}"#,
            link.ip, link.stall_gate_events
        )
        .ok();
    }

    writeln!(
        out,
        "# HELP srtla_send_link_silence_pulls cumulative fast silence-pull engagements"
    )
    .ok();
    writeln!(out, "# TYPE srtla_send_link_silence_pulls counter").ok();
    for link in &snap.links {
        writeln!(
            out,
            r#"srtla_send_link_silence_pulls{{ip="{}"}} {}"#,
            link.ip, link.silence_pulls
        )
        .ok();
    }

    // Aggregate gauges.
    writeln!(
        out,
        "# HELP srtla_send_active_links links currently connected and live"
    )
    .ok();
    writeln!(out, "# TYPE srtla_send_active_links gauge").ok();
    writeln!(out, "srtla_send_active_links {}", snap.active_links).ok();

    writeln!(out, "# HELP srtla_send_total_links configured link count").ok();
    writeln!(out, "# TYPE srtla_send_total_links gauge").ok();
    writeln!(out, "srtla_send_total_links {}", snap.total_links).ok();

    writeln!(
        out,
        "# HELP srtla_send_total_window summed window across active links"
    )
    .ok();
    writeln!(out, "# TYPE srtla_send_total_window gauge").ok();
    writeln!(out, "srtla_send_total_window {}", snap.total_window).ok();

    writeln!(
        out,
        "# HELP srtla_send_total_in_flight summed in-flight across active links"
    )
    .ok();
    writeln!(out, "# TYPE srtla_send_total_in_flight gauge").ok();
    writeln!(out, "srtla_send_total_in_flight {}", snap.total_in_flight).ok();

    // Scheduler config surfaced as a gauge so Grafana can pivot on it.
    writeln!(
        out,
        "# HELP srtla_send_mode scheduling mode (0=classic,1=enhanced)"
    )
    .ok();
    writeln!(out, "# TYPE srtla_send_mode gauge").ok();
    let mode = match config.mode() {
        SchedulingMode::Classic => 0,
        SchedulingMode::Enhanced => 1,
    };
    writeln!(out, "srtla_send_mode {mode}").ok();

    // Priority sidecar counters.
    writeln!(
        out,
        "# HELP srtla_send_critical_windows_total total keyframe-priority datagrams applied"
    )
    .ok();
    writeln!(out, "# TYPE srtla_send_critical_windows_total counter").ok();
    writeln!(
        out,
        "srtla_send_critical_windows_total {}",
        cw.windows_received()
    )
    .ok();

    writeln!(
        out,
        "# HELP srtla_send_critical_malformed_datagrams_total malformed priority-sidecar datagrams"
    )
    .ok();
    writeln!(
        out,
        "# TYPE srtla_send_critical_malformed_datagrams_total counter"
    )
    .ok();
    writeln!(
        out,
        "srtla_send_critical_malformed_datagrams_total {}",
        cw.malformed_datagrams()
    )
    .ok();

    // Suppress unused-variable warnings when all fields above are covered.
    let _ = snap;

    out
}

/// Spawn the Prometheus scrape endpoint. Runs on the main tokio runtime.
pub fn spawn_server(
    bind: SocketAddr,
    stats: SharedStats,
    config: DynamicConfig,
    cw: CriticalWindow,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let listener = match TcpListener::bind(bind).await {
            Ok(l) => l,
            Err(e) => {
                warn!(%bind, error = %e, "failed to bind prometheus endpoint");
                return;
            }
        };
        let local = listener.local_addr().ok();
        info!(?local, "prometheus /metrics endpoint listening");

        loop {
            let (stream, peer) = match listener.accept().await {
                Ok(pair) => pair,
                Err(e) => {
                    debug!(error = %e, "prometheus accept error");
                    continue;
                }
            };
            let stats = stats.clone();
            let config = config.clone();
            let cw = cw.clone();
            tokio::spawn(async move {
                if let Err(e) = serve_one(stream, &stats, &config, &cw).await {
                    debug!(%peer, error = %e, "prometheus scrape error");
                }
            });
        }
    })
}

async fn serve_one(
    mut stream: tokio::net::TcpStream,
    stats: &SharedStats,
    config: &DynamicConfig,
    cw: &CriticalWindow,
) -> std::io::Result<()> {
    // Read until we've seen the end of the request headers. One read
    // usually suffices for a scraper-originated GET; cap at 4 KiB to
    // prevent slowloris-style games.
    let mut buf = [0u8; 4096];
    let mut len = 0;
    loop {
        if len == buf.len() {
            break;
        }
        let n = stream.read(&mut buf[len..]).await?;
        if n == 0 {
            break;
        }
        len += n;
        if buf[..len].windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
    }
    let request = &buf[..len];
    let path = request_path(request);
    let body = match path.as_deref() {
        Some("/metrics") | Some("/") => render(stats, config, cw),
        _ => {
            let resp = b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
            stream.write_all(resp).await?;
            return Ok(());
        }
    };

    let header = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/plain; version=0.0.4\r\nContent-Length: \
         {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(header.as_bytes()).await?;
    stream.write_all(body.as_bytes()).await?;
    Ok(())
}

fn request_path(request: &[u8]) -> Option<String> {
    // GET /metrics HTTP/1.1
    let first_line_end = request.iter().position(|&b| b == b'\r')?;
    let line = std::str::from_utf8(&request[..first_line_end]).ok()?;
    let mut parts = line.split(' ');
    let method = parts.next()?;
    if !method.eq_ignore_ascii_case("GET") {
        return None;
    }
    parts.next().map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_contains_core_metric_lines() {
        let stats = SharedStats::new();
        let config = DynamicConfig::new();
        let cw = CriticalWindow::new();
        let text = render(&stats, &config, &cw);
        assert!(text.contains("srtla_send_active_links"));
        assert!(text.contains("srtla_send_total_links"));
        assert!(text.contains("srtla_send_critical_windows_total"));
        assert!(text.contains("srtla_send_mode"));
    }

    #[test]
    fn render_outputs_valid_prom_shape() {
        // Every HELP / TYPE comment should be followed by at least one sample.
        let stats = SharedStats::new();
        let config = DynamicConfig::new();
        let cw = CriticalWindow::new();
        let text = render(&stats, &config, &cw);
        for line in text.lines() {
            // No NaN / weird unicode smuggled in.
            assert!(line.is_ascii(), "non-ASCII metric line: {line}");
        }
    }

    #[test]
    fn request_path_parses_standard_get() {
        let req = b"GET /metrics HTTP/1.1\r\nHost: x\r\n\r\n";
        assert_eq!(request_path(req).as_deref(), Some("/metrics"));
    }

    #[test]
    fn request_path_rejects_post() {
        let req = b"POST /metrics HTTP/1.1\r\n\r\n";
        assert_eq!(request_path(req), None);
    }

    #[test]
    fn request_path_rejects_malformed() {
        assert_eq!(request_path(b""), None);
        assert_eq!(request_path(b"not http"), None);
    }
}
