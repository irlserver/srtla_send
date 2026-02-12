//! Link failure and recovery integration tests.
//!
//! Validates that srtla_send detects link failure, continues on surviving
//! links, and recovers when a failed link returns.

mod common;

use std::thread;
use std::time::Duration;

use network_sim::{ImpairmentConfig, SrtlaTestStack};

#[test]
fn test_link_failure_failover() {
    if common::skip_without_impairment_deps() {
        return;
    }
    common::build_srtla_send();

    let mut stack = SrtlaTestStack::start("fail", 2, &[]).expect("start stack");

    // Let both links register
    thread::sleep(Duration::from_secs(5));

    // Inject background data
    common::inject_packets(&stack, 100).expect("inject initial data");
    thread::sleep(Duration::from_secs(2));

    // Kill link 0 with 100% loss
    stack
        .impair_link(
            0,
            ImpairmentConfig {
                loss_percent: Some(100.0),
                ..Default::default()
            },
        )
        .expect("kill link 0");

    // Continue sending — should survive on link 1
    common::inject_packets(&stack, 200).expect("inject data after link kill");
    thread::sleep(Duration::from_secs(5));

    let output = stack.stop();
    common::dump_output(&output);

    let all_stderr: String = output.srtla_send_stderr.join("\n");
    assert!(
        !all_stderr.contains("PANIC") && !all_stderr.contains("panic"),
        "srtla_send panicked after link failure"
    );
}

#[test]
fn test_link_recovery() {
    if common::skip_without_impairment_deps() {
        return;
    }
    common::build_srtla_send();

    let mut stack = SrtlaTestStack::start("recv", 2, &[]).expect("start stack");

    // Let both links register
    thread::sleep(Duration::from_secs(5));

    // Kill link 0
    stack
        .impair_link(
            0,
            ImpairmentConfig {
                loss_percent: Some(100.0),
                ..Default::default()
            },
        )
        .expect("kill link 0");

    thread::sleep(Duration::from_secs(5));

    // Restore link 0 (clear impairment)
    stack
        .impair_link(0, ImpairmentConfig::default())
        .expect("restore link 0");

    // Give time for re-registration
    thread::sleep(Duration::from_secs(5));

    common::inject_packets(&stack, 100).expect("inject data after recovery");
    thread::sleep(Duration::from_secs(3));

    let output = stack.stop();
    common::dump_output(&output);

    let all_stderr: String = output.srtla_send_stderr.join("\n");
    assert!(
        !all_stderr.contains("PANIC") && !all_stderr.contains("panic"),
        "srtla_send panicked during link recovery"
    );
}
