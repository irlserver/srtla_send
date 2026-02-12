//! Shared utilities for integration tests.
#![allow(dead_code)]

use std::time::Duration;

use network_sim::{SrtlaTestStack, check_impairment_deps, check_integration_deps};

/// Check all integration test dependencies. Returns `true` if tests should
/// be skipped (prints the reason to stderr). Use at the top of every test.
pub fn skip_without_deps() -> bool {
    match check_integration_deps() {
        Ok(()) => false,
        Err(reason) => {
            eprintln!("Skipping: {reason}");
            true
        }
    }
}

/// Like `skip_without_deps` but also requires netem for impairment tests.
pub fn skip_without_impairment_deps() -> bool {
    match check_impairment_deps() {
        Ok(()) => false,
        Err(reason) => {
            eprintln!("Skipping: {reason}");
            true
        }
    }
}

/// Build the srtla_send binary (debug mode). Call once before tests that
/// need the binary. Panics if the build fails.
pub fn build_srtla_send() {
    let status = std::process::Command::new("cargo")
        .args(["build", "--bin", "srtla_send"])
        .status()
        .expect("failed to run cargo build");
    assert!(status.success(), "cargo build failed");
}

/// Inject `count` UDP packets to srtla_send's local SRT port from within
/// the sender namespace. Each packet is 188 bytes of zeroes.
pub fn inject_packets(stack: &SrtlaTestStack, count: usize) -> anyhow::Result<()> {
    network_sim::inject_udp_packets(
        &stack.topo.sender_ns,
        "127.0.0.1",
        stack.sender_srt_port(),
        count,
    )
}

/// Inject a steady UDP stream into srtla_send for `duration`.
#[allow(dead_code)]
pub fn inject_stream(
    stack: &SrtlaTestStack,
    packets_per_sec: u32,
    duration: Duration,
) -> anyhow::Result<()> {
    network_sim::inject_udp_stream(
        &stack.topo.sender_ns,
        "127.0.0.1",
        stack.sender_srt_port(),
        packets_per_sec,
        duration,
    )
}

/// Collect and print all process output for debugging failed tests.
pub fn dump_output(output: &network_sim::StackOutput) {
    eprintln!("--- srtla_send stdout ---");
    for line in &output.srtla_send_stdout {
        eprintln!("  {line}");
    }
    eprintln!("--- srtla_send stderr ---");
    for line in &output.srtla_send_stderr {
        eprintln!("  {line}");
    }
    eprintln!("--- srtla_rec stdout ---");
    for line in &output.srtla_rec_stdout {
        eprintln!("  {line}");
    }
    eprintln!("--- srtla_rec stderr ---");
    for line in &output.srtla_rec_stderr {
        eprintln!("  {line}");
    }
    eprintln!("--- srt-live-transmit stdout ---");
    for line in &output.srt_server_stdout {
        eprintln!("  {line}");
    }
    eprintln!("--- srt-live-transmit stderr ---");
    for line in &output.srt_server_stderr {
        eprintln!("  {line}");
    }
}
