//! Impaired network integration tests.
//!
//! Validates that srtla_send adapts to asymmetric delay, packet loss,
//! and bandwidth limits.

mod common;

use std::thread;
use std::time::Duration;

use network_sim::{ImpairmentConfig, SrtlaTestStack};

#[test]
fn test_asymmetric_delay() {
    if common::skip_without_impairment_deps() {
        return;
    }
    common::build_srtla_send();

    let mut stack = SrtlaTestStack::start("asym", 2, &[]).expect("start stack");

    // Link 0: low delay, Link 1: high delay
    stack
        .impair_link(
            0,
            ImpairmentConfig {
                delay_ms: Some(20),
                ..Default::default()
            },
        )
        .expect("impair link 0");

    stack
        .impair_link(
            1,
            ImpairmentConfig {
                delay_ms: Some(100),
                ..Default::default()
            },
        )
        .expect("impair link 1");

    // Wait for registration + RTT measurement
    thread::sleep(Duration::from_secs(5));

    // Inject some data so RTT tracking kicks in
    common::inject_packets(&stack, 200).expect("inject packets");
    thread::sleep(Duration::from_secs(5));

    let output = stack.stop();
    common::dump_output(&output);

    let all_stderr: String = output.srtla_send_stderr.join("\n");
    assert!(
        !all_stderr.contains("PANIC") && !all_stderr.contains("panic"),
        "srtla_send panicked"
    );
}

#[test]
fn test_loss_triggers_window_reduction() {
    if common::skip_without_impairment_deps() {
        return;
    }
    common::build_srtla_send();

    let mut stack = SrtlaTestStack::start("loss", 2, &[]).expect("start stack");

    // Wait for clean registration first
    thread::sleep(Duration::from_secs(5));

    // Apply 10% loss on link 0
    stack
        .impair_link(
            0,
            ImpairmentConfig {
                loss_percent: Some(10.0),
                ..Default::default()
            },
        )
        .expect("impair link 0 with loss");

    // Inject data to trigger NAK detection
    common::inject_packets(&stack, 500).expect("inject packets");
    thread::sleep(Duration::from_secs(5));

    let output = stack.stop();
    common::dump_output(&output);

    let all_stderr: String = output.srtla_send_stderr.join("\n");
    assert!(
        !all_stderr.contains("PANIC") && !all_stderr.contains("panic"),
        "srtla_send panicked"
    );
}

#[test]
fn test_tbf_bandwidth_limit() {
    if common::skip_without_impairment_deps() {
        return;
    }
    common::build_srtla_send();

    let mut stack = SrtlaTestStack::start("tbf", 2, &[]).expect("start stack");

    // Link 0: 1 Mbps, Link 1: 5 Mbps
    stack
        .impair_link(
            0,
            ImpairmentConfig {
                rate_kbit: Some(1000),
                tbf_shaping: true,
                ..Default::default()
            },
        )
        .expect("impair link 0");

    stack
        .impair_link(
            1,
            ImpairmentConfig {
                rate_kbit: Some(5000),
                tbf_shaping: true,
                ..Default::default()
            },
        )
        .expect("impair link 1");

    thread::sleep(Duration::from_secs(5));

    // Inject a burst of data
    common::inject_packets(&stack, 500).expect("inject packets");
    thread::sleep(Duration::from_secs(5));

    let output = stack.stop();
    common::dump_output(&output);

    let all_stderr: String = output.srtla_send_stderr.join("\n");
    assert!(
        !all_stderr.contains("PANIC") && !all_stderr.contains("panic"),
        "srtla_send panicked under bandwidth constraints"
    );
}
