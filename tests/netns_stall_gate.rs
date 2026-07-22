//! Stalled-link deselect end-to-end test.
//!
//! Validates the stall-latch lifecycle over a real bond: a black-holed link
//! is pulled from rotation (latch engages), the stream survives on the
//! healthy link, and once the link heals the latch releases only after the
//! rejoin dwell. The release assertion also exercises the duplicate-probe
//! path end-to-end: on a near-zero-RTT veth pair the keepalive-RTT probe is
//! armed only every ~3 s, which is sparser than the staleness window, so the
//! probes' earned SRTLA ACKs are what keep the recovery run's delivery proof
//! continuously fresh enough to complete the dwell.

mod common;

use std::time::Duration;

use network_sim::{ImpairmentConfig, SrtlaTestStack};

/// Lowered staleness ceiling. The latch must engage and the blackhole must
/// end well inside CONN_TIMEOUT (5 s): a timed-out link resets its latch, and
/// the whole point here is observing the latch survive to a warm rejoin.
const STALE_CEILING_MS: &str = "1500";

#[test]
fn test_stall_gate_latches_and_rejoins_after_dwell() {
    if common::skip_without_impairment_deps() {
        return;
    }
    common::build_srtla_send();

    let mut stack = SrtlaTestStack::start("stall", 2, &["--stall-ack-stale-ms", STALE_CEILING_MS])
        .expect("start stack");
    common::wait_until_ready(&stack);

    // Continuous load throughout: selection (which drives the latch) runs per
    // routed packet, and the black-holed link must accumulate the >=32
    // in-flight backlog the stall signal requires.
    common::inject_stream(&stack, 400, Duration::from_secs(2)).expect("warmup stream");

    stack
        .impair_link(
            0,
            ImpairmentConfig {
                loss_percent: Some(100.0),
                ..Default::default()
            },
        )
        .expect("blackhole link 0");

    // Latch expected after ~1.5 s of proof staleness; 3 s of load leaves
    // margin while keeping the blackhole shorter than CONN_TIMEOUT.
    common::inject_stream(&stack, 400, Duration::from_secs(3)).expect("stream during blackhole");

    stack
        .impair_link(0, ImpairmentConfig::default())
        .expect("restore link 0");

    // Rejoin dwell is 2x the effective staleness window (3 s here). Keep the
    // stream up so duplicate probes earn the gated link continuous ACK proof.
    common::inject_stream(&stack, 400, Duration::from_secs(8)).expect("stream through recovery");

    let output = stack.stop();
    common::dump_output(&output);

    let stderr: String = output.srtla_send_stderr.join("\n");
    assert!(
        stderr.contains("silence pull engaged"),
        "black-holed link never hit the fast silence pull"
    );
    assert!(
        stderr.contains("stall latch engaged"),
        "black-holed link never engaged the stall latch"
    );
    assert!(
        stderr.contains("stall latch released after sustained proof"),
        "healed link never completed the rejoin dwell (is duplicate probing delivering proof?)"
    );
    assert!(
        !stderr.contains("PANIC") && !stderr.contains("panic"),
        "srtla_send panicked during the stall-gate lifecycle"
    );
}
