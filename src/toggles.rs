use std::io::{BufRead, BufReader};
#[cfg(unix)]
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

#[cfg(unix)]
use tracing::debug;
use tracing::{info, warn};

#[derive(Clone)]
pub struct DynamicToggles {
    pub classic_mode: Arc<AtomicBool>,
    pub stickiness_enabled: Arc<AtomicBool>,
    pub quality_scoring_enabled: Arc<AtomicBool>,
    pub exploration_enabled: Arc<AtomicBool>,
}

impl DynamicToggles {
    #[allow(dead_code)]
    pub fn new() -> Self {
        Self {
            classic_mode: Arc::new(AtomicBool::new(false)),
            stickiness_enabled: Arc::new(AtomicBool::new(true)),
            quality_scoring_enabled: Arc::new(AtomicBool::new(true)),
            exploration_enabled: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn from_cli(
        classic: bool,
        no_stickiness: bool,
        no_quality: bool,
        exploration: bool,
    ) -> Self {
        Self {
            classic_mode: Arc::new(AtomicBool::new(classic)),
            stickiness_enabled: Arc::new(AtomicBool::new(!no_stickiness)),
            quality_scoring_enabled: Arc::new(AtomicBool::new(!no_quality)),
            exploration_enabled: Arc::new(AtomicBool::new(exploration)),
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
            for line in reader.lines() {
                if let Ok(cmd) = line {
                    apply_cmd(&toggles_clone, cmd.trim());
                }
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

fn apply_cmd(toggles: &DynamicToggles, cmd: &str) {
    let cmd = cmd.trim();
    match cmd {
        "classic on" | "classic=true" => {
            toggles.classic_mode.store(true, Ordering::Relaxed);
            info!("🔧 Classic mode: ON");
        }
        "classic off" | "classic=false" => {
            toggles.classic_mode.store(false, Ordering::Relaxed);
            info!("🔧 Classic mode: OFF");
        }
        "stick on" | "stickiness=true" => {
            toggles.stickiness_enabled.store(true, Ordering::Relaxed);
            info!("🔧 Stickiness: ON");
        }
        "stick off" | "stickiness=false" => {
            toggles.stickiness_enabled.store(false, Ordering::Relaxed);
            info!("🔧 Stickiness: OFF");
        }
        "quality on" | "quality=true" => {
            toggles
                .quality_scoring_enabled
                .store(true, Ordering::Relaxed);
            info!("🔧 Quality scoring: ON");
        }
        "quality off" | "quality=false" => {
            toggles
                .quality_scoring_enabled
                .store(false, Ordering::Relaxed);
            info!("🔧 Quality scoring: OFF");
        }
        "explore on" | "exploration=true" => {
            toggles.exploration_enabled.store(true, Ordering::Relaxed);
            info!("🔧 Exploration: ON");
        }
        "explore off" | "exploration=false" => {
            toggles.exploration_enabled.store(false, Ordering::Relaxed);
            info!("🔧 Exploration: OFF");
        }
        "status" => {
            info!("📊 Current toggles:");
            info!(
                "  Classic mode: {}",
                toggles.classic_mode.load(Ordering::Relaxed)
            );
            info!(
                "  Stickiness: {}",
                toggles.stickiness_enabled.load(Ordering::Relaxed)
            );
            info!(
                "  Quality scoring: {}",
                toggles.quality_scoring_enabled.load(Ordering::Relaxed)
            );
            info!(
                "  Exploration: {}",
                toggles.exploration_enabled.load(Ordering::Relaxed)
            );
        }
        "" => {} // ignore empty lines
        _ => warn!("❌ Unknown command: {}", cmd),
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
