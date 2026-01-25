use std::io::{BufRead, BufReader};
#[cfg(unix)]
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

#[cfg(unix)]
use tracing::debug;
use tracing::{info, warn};

/// Default RTT delta threshold in milliseconds.
/// Links within min_rtt + delta are considered "fast" and preferred.
pub const DEFAULT_RTT_DELTA_MS: u32 = 30;

/// Snapshot of toggle states for efficient hot-path access.
/// This avoids multiple atomic loads per packet by caching the values once per select iteration.
#[derive(Clone, Copy, Debug)]
pub struct ToggleSnapshot {
    pub classic_mode: bool,
    pub quality_scoring_enabled: bool,
    pub exploration_enabled: bool,
    pub rtt_threshold_enabled: bool,
    pub rtt_delta_ms: u32,
}

#[derive(Clone)]
pub struct DynamicToggles {
    pub classic_mode: Arc<AtomicBool>,
    pub quality_scoring_enabled: Arc<AtomicBool>,
    pub exploration_enabled: Arc<AtomicBool>,
    pub rtt_threshold_enabled: Arc<AtomicBool>,
    pub rtt_delta_ms: Arc<AtomicU32>,
}

impl Default for DynamicToggles {
    fn default() -> Self {
        Self::new()
    }
}

impl DynamicToggles {
    pub fn new() -> Self {
        Self {
            classic_mode: Arc::new(AtomicBool::new(false)),
            quality_scoring_enabled: Arc::new(AtomicBool::new(true)),
            exploration_enabled: Arc::new(AtomicBool::new(false)),
            rtt_threshold_enabled: Arc::new(AtomicBool::new(false)),
            rtt_delta_ms: Arc::new(AtomicU32::new(DEFAULT_RTT_DELTA_MS)),
        }
    }

    pub fn from_cli(
        classic: bool,
        no_quality: bool,
        exploration: bool,
        rtt_threshold: bool,
        rtt_delta_ms: u32,
    ) -> Self {
        Self {
            classic_mode: Arc::new(AtomicBool::new(classic)),
            quality_scoring_enabled: Arc::new(AtomicBool::new(!no_quality)),
            exploration_enabled: Arc::new(AtomicBool::new(exploration)),
            rtt_threshold_enabled: Arc::new(AtomicBool::new(rtt_threshold)),
            rtt_delta_ms: Arc::new(AtomicU32::new(rtt_delta_ms)),
        }
    }

    /// Create a snapshot of current toggle states.
    /// Call this once at the start of each select iteration to avoid
    /// multiple atomic loads per packet in the hot path.
    #[inline]
    pub fn snapshot(&self) -> ToggleSnapshot {
        ToggleSnapshot {
            classic_mode: self.classic_mode.load(Ordering::Relaxed),
            quality_scoring_enabled: self.quality_scoring_enabled.load(Ordering::Relaxed),
            exploration_enabled: self.exploration_enabled.load(Ordering::Relaxed),
            rtt_threshold_enabled: self.rtt_threshold_enabled.load(Ordering::Relaxed),
            rtt_delta_ms: self.rtt_delta_ms.load(Ordering::Relaxed),
        }
    }
}

pub fn spawn_toggle_listener(toggles: DynamicToggles, socket_path: Option<String>) {
    // Spawn stdin listener (backward compatibility)
    if socket_path.is_none() {
        let toggles_clone = toggles.clone();
        std::thread::spawn(move || {
            let stdin = std::io::stdin();
            let reader = BufReader::new(stdin);
            for cmd in reader.lines().map_while(Result::ok) {
                apply_cmd(&toggles_clone, cmd.trim());
            }
        });
    }

    // Spawn Unix domain socket listener if socket path specified
    #[cfg(unix)]
    if let Some(sock_path) = socket_path {
        let toggles_clone = toggles.clone();
        std::thread::spawn(move || {
            unix_socket_loop(&toggles_clone, &sock_path);
        });
    }
}

pub fn apply_cmd(toggles: &DynamicToggles, cmd: &str) {
    let cmd = cmd.trim();
    match cmd {
        "classic on" | "classic=true" => {
            toggles.classic_mode.store(true, Ordering::Relaxed);
            info!("Classic mode: ON");
        }
        "classic off" | "classic=false" => {
            toggles.classic_mode.store(false, Ordering::Relaxed);
            info!("Classic mode: OFF");
        }
        "quality on" | "quality=true" => {
            toggles
                .quality_scoring_enabled
                .store(true, Ordering::Relaxed);
            info!("Quality scoring: ON");
        }
        "quality off" | "quality=false" => {
            toggles
                .quality_scoring_enabled
                .store(false, Ordering::Relaxed);
            info!("Quality scoring: OFF");
        }
        "explore on" | "exploration=true" => {
            toggles.exploration_enabled.store(true, Ordering::Relaxed);
            info!("Exploration: ON");
        }
        "explore off" | "exploration=false" => {
            toggles.exploration_enabled.store(false, Ordering::Relaxed);
            info!("Exploration: OFF");
        }
        "rtt on" | "rtt=true" | "rtt_threshold on" | "rtt_threshold=true" => {
            toggles.rtt_threshold_enabled.store(true, Ordering::Relaxed);
            info!("RTT-threshold mode: ON");
        }
        "rtt off" | "rtt=false" | "rtt_threshold off" | "rtt_threshold=false" => {
            toggles
                .rtt_threshold_enabled
                .store(false, Ordering::Relaxed);
            info!("RTT-threshold mode: OFF");
        }
        "status" => {
            info!("Current toggles:");
            info!(
                "  Classic mode: {}",
                toggles.classic_mode.load(Ordering::Relaxed)
            );
            info!(
                "  Quality scoring: {}",
                toggles.quality_scoring_enabled.load(Ordering::Relaxed)
            );
            info!(
                "  Exploration: {}",
                toggles.exploration_enabled.load(Ordering::Relaxed)
            );
            info!(
                "  RTT-threshold: {}",
                toggles.rtt_threshold_enabled.load(Ordering::Relaxed)
            );
            info!(
                "  RTT delta: {}ms",
                toggles.rtt_delta_ms.load(Ordering::Relaxed)
            );
        }
        "" => {}
        _ => {
            // Handle rtt_delta=N command
            if let Some(delta_str) = cmd.strip_prefix("rtt_delta=") {
                if let Ok(delta) = delta_str.parse::<u32>() {
                    toggles.rtt_delta_ms.store(delta, Ordering::Relaxed);
                    info!("RTT delta: {}ms", delta);
                } else {
                    warn!("Invalid rtt_delta value: {}", delta_str);
                }
            } else {
                warn!("Unknown command: {}", cmd);
            }
        }
    }
}

#[cfg(unix)]
fn unix_socket_loop(toggles: &DynamicToggles, socket_path: &str) {
    // Remove existing socket file if it exists
    let _ = std::fs::remove_file(socket_path);

    let listener = match UnixListener::bind(socket_path) {
        Ok(l) => l,
        Err(e) => {
            warn!("Failed to bind Unix socket {}: {}", socket_path, e);
            return;
        }
    };

    info!("🔌 Unix socket listening at: {}", socket_path);
    info!(
        "💡 Send commands like: echo 'classic on' | socat - UNIX-CONNECT:{}",
        socket_path
    );

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let toggles_clone = toggles.clone();
                std::thread::spawn(move || {
                    handle_unix_client(toggles_clone, stream);
                });
            }
            Err(e) => {
                debug!("Unix socket accept error: {}", e);
            }
        }
    }
}

#[cfg(unix)]
fn handle_unix_client(toggles: DynamicToggles, stream: UnixStream) {
    let reader = BufReader::new(&stream);
    for line in reader.lines() {
        match line {
            Ok(cmd) => {
                apply_cmd(&toggles, cmd.trim());
            }
            Err(_) => break, // Connection closed
        }
    }
}
