//! Scenario-driven integration tests.
//!
//! Uses the random-walk scenario generator to apply evolving impairment
//! over time and validates that srtla_send survives without crashing.

mod common;

use std::thread;
use std::time::{Duration, Instant};

use network_sim::{ImpairmentConfig, LinkScenarioConfig, Scenario, ScenarioConfig, SrtlaTestStack};

#[test]
fn test_random_walk_stability() {
    if common::skip_without_impairment_deps() {
        return;
    }
    common::build_srtla_send();

    let mut stack = SrtlaTestStack::start("rw", 2, &[]).expect("start stack");

    // Wait for registration
    thread::sleep(Duration::from_secs(5));

    let scenario_cfg = ScenarioConfig {
        seed: 42,
        duration: Duration::from_secs(30),
        step: Duration::from_secs(2),
        links: vec![
            LinkScenarioConfig {
                min_rate_kbit: 500,
                max_rate_kbit: 5000,
                rate_step_kbit: 500,
                base_delay_ms: 20,
                delay_jitter_ms: 30,
                delay_step_ms: 5,
                max_loss_percent: 5.0,
                loss_step_percent: 1.0,
            },
            LinkScenarioConfig {
                min_rate_kbit: 1000,
                max_rate_kbit: 8000,
                rate_step_kbit: 800,
                base_delay_ms: 15,
                delay_jitter_ms: 20,
                delay_step_ms: 4,
                max_loss_percent: 3.0,
                loss_step_percent: 0.5,
            },
        ],
    };

    let frames = Scenario::new(scenario_cfg).frames();
    let start = Instant::now();

    // Inject a slow background stream while applying scenario
    let inject_handle = {
        // Spawn injection in a separate thread
        let sender_ns_name = stack.topo.sender_ns.name.clone();
        let port = stack.sender_srt_port();
        thread::spawn(move || {
            // Inject packets periodically for the duration
            let end = Instant::now() + Duration::from_secs(30);
            while Instant::now() < end {
                // Use a raw command since we don't have the Namespace object
                let _ = std::process::Command::new("sudo")
                    .args([
                        "ip",
                        "netns",
                        "exec",
                        &sender_ns_name,
                        "python3",
                        "-c",
                        &format!(
                            "import socket; s=socket.socket(socket.AF_INET,socket.SOCK_DGRAM); \
                             [s.sendto(b'\\x00'*188,('127.0.0.1',{port})) for _ in range(50)]; \
                             s.close()"
                        ),
                    ])
                    .output();
                thread::sleep(Duration::from_millis(500));
            }
        })
    };

    for frame in &frames {
        let elapsed = start.elapsed();
        if elapsed < frame.t {
            thread::sleep(frame.t - elapsed);
        }
        for (i, cfg) in frame.configs.iter().enumerate() {
            if let Err(e) = stack.impair_link(i, cfg.clone()) {
                eprintln!("impair_link({i}) at t={:?}: {e}", frame.t);
            }
        }
    }

    let _ = inject_handle.join();

    let output = stack.stop();
    common::dump_output(&output);

    let all_stderr: String = output.srtla_send_stderr.join("\n");
    assert!(
        !all_stderr.contains("PANIC") && !all_stderr.contains("panic"),
        "srtla_send panicked during random-walk scenario"
    );
}

#[test]
fn test_step_change_convergence() {
    if common::skip_without_impairment_deps() {
        return;
    }
    common::build_srtla_send();

    let mut stack = SrtlaTestStack::start("step", 2, &[]).expect("start stack");

    // Wait for registration
    thread::sleep(Duration::from_secs(5));

    // Phase 1: Stable, good conditions (5s)
    stack
        .impair_link(
            0,
            ImpairmentConfig {
                rate_kbit: Some(5000),
                delay_ms: Some(10),
                ..Default::default()
            },
        )
        .expect("set good conditions link 0");
    stack
        .impair_link(
            1,
            ImpairmentConfig {
                rate_kbit: Some(5000),
                delay_ms: Some(10),
                ..Default::default()
            },
        )
        .expect("set good conditions link 1");

    common::inject_packets(&stack, 200).expect("inject data phase 1");
    thread::sleep(Duration::from_secs(5));

    // Phase 2: Sudden bandwidth drop on link 0 (5s)
    stack
        .impair_link(
            0,
            ImpairmentConfig {
                rate_kbit: Some(500),
                delay_ms: Some(50),
                tbf_shaping: true,
                ..Default::default()
            },
        )
        .expect("step-change link 0");

    common::inject_packets(&stack, 200).expect("inject data phase 2");
    thread::sleep(Duration::from_secs(5));

    // Phase 3: Recover (5s)
    stack
        .impair_link(
            0,
            ImpairmentConfig {
                rate_kbit: Some(5000),
                delay_ms: Some(10),
                ..Default::default()
            },
        )
        .expect("recover link 0");

    common::inject_packets(&stack, 200).expect("inject data phase 3");
    thread::sleep(Duration::from_secs(5));

    let output = stack.stop();
    common::dump_output(&output);

    let all_stderr: String = output.srtla_send_stderr.join("\n");
    assert!(
        !all_stderr.contains("PANIC") && !all_stderr.contains("panic"),
        "srtla_send panicked during step-change test"
    );
}
