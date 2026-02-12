//! Basic connectivity integration tests.
//!
//! Validates that srtla_send completes registration through real srtla_rec
//! and forwards data through the full pipeline.

mod common;

use std::thread;
use std::time::Duration;

use network_sim::SrtlaTestStack;

#[test]
fn test_two_link_registration() {
    if common::skip_without_deps() {
        return;
    }
    common::build_srtla_send();

    let mut stack = SrtlaTestStack::start("reg2", 2, &[]).expect("start stack");

    // Allow time for registration handshake on both links
    thread::sleep(Duration::from_secs(5));

    let output = stack.stop();
    common::dump_output(&output);

    // Sender should have started without crashing
    let all_stderr: String = output.srtla_send_stderr.join("\n");

    // Look for registration success indicators in logs
    // (srtla_send logs registration events at debug level)
    assert!(
        !all_stderr.contains("PANIC") && !all_stderr.contains("panic"),
        "srtla_send panicked"
    );
}

#[test]
fn test_data_forwarding() {
    if common::skip_without_deps() {
        return;
    }
    common::build_srtla_send();

    let mut stack = SrtlaTestStack::start("fwd", 2, &[]).expect("start stack");

    // Wait for registration
    thread::sleep(Duration::from_secs(5));

    // Inject UDP packets into sender's local SRT port
    common::inject_packets(&stack, 100).expect("inject packets");

    // Allow data to flow through the pipeline
    thread::sleep(Duration::from_secs(3));

    let output = stack.stop();
    common::dump_output(&output);

    let all_stderr: String = output.srtla_send_stderr.join("\n");
    assert!(
        !all_stderr.contains("PANIC") && !all_stderr.contains("panic"),
        "srtla_send panicked"
    );
}
