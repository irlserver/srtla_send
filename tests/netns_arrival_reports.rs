//! Per-link arrival reports against a real srtla_rec.
//!
//! The sender counts the sequence entries of each SRTLA ACK on the link it
//! arrived on. That is only meaningful if the receiver really reports a packet
//! on the link that delivered it, so this runs a real bond: on clean links
//! each link's reported entries must match what it queued, up to one
//! receiver batch, and a link whose path is black-holed must stop gaining
//! reports while its sibling keeps them coming.
//!
//! The clean-phase comparison assumes a drained, lossless phase: unimpaired
//! veths, the stream stopped and a pause before reading. It checks the
//! receiver's attribution, not the telemetry contract at every instant (where
//! several batches can be in flight and return loss removes reports).

mod common;

use std::thread;
use std::time::Duration;

use network_sim::ImpairmentConfig;

/// The reference receivers acknowledge in batches of this many sequences, so
/// up to one batch less per link can be outstanding at any moment.
const RECEIVER_ACK_BATCH: u64 = 10;

fn link_counts(stats: &serde_json::Value) -> Vec<(u64, u64, u64)> {
    stats["links"]
        .as_array()
        .expect("links array")
        .iter()
        .map(|l| {
            (
                l["data_packets_queued_total"].as_u64().expect("queued"),
                l["srtla_ack_entries_total"].as_u64().expect("entries"),
                l["srtla_ack_epoch"].as_u64().expect("epoch"),
            )
        })
        .collect()
}

#[test]
fn test_arrival_reports_follow_the_delivering_link() {
    if common::skip_without_impairment_deps() {
        return;
    }
    let sock = format!("/tmp/srtla-arrival-{}.sock", std::process::id());
    let mut stack =
        common::start_stack("arrive", 2, &["--control-socket", &sock]).expect("start stack");
    common::wait_until_ready(&stack);

    common::inject_stream(&stack, 400, Duration::from_secs(4)).expect("clean stream");
    // Let the last receiver batches come back before reading.
    thread::sleep(Duration::from_secs(1));
    let clean = link_counts(&stack.get_stats(&sock).expect("stats after clean run"));
    eprintln!("clean (queued, entries, epoch) per link: {clean:?}");
    assert_eq!(clean.len(), 2);
    assert!(
        clean.iter().map(|c| c.0).sum::<u64>() >= 1000,
        "the bond carried too little to judge: {clean:?}"
    );
    for (i, &(queued, entries, _)) in clean.iter().enumerate() {
        assert!(
            queued > 0 && entries > 0,
            "link {i} carried nothing in the clean phase, so it cannot be judged: {clean:?}"
        );
        assert!(
            entries <= queued,
            "link {i} reported more arrivals than it queued: {clean:?}"
        );
        assert!(
            queued - entries < RECEIVER_ACK_BATCH,
            "link {i} is missing more than one receiver batch of reports: {clean:?}"
        );
    }

    stack
        .impair_link_both_ways(
            0,
            ImpairmentConfig {
                loss_percent: Some(100.0),
                ..Default::default()
            },
        )
        .expect("blackhole link 0");
    common::inject_stream(&stack, 400, Duration::from_secs(2)).expect("stream during blackhole");
    thread::sleep(Duration::from_millis(500));
    let dark = link_counts(&stack.get_stats(&sock).expect("stats during blackhole"));
    eprintln!("blackhole (queued, entries, epoch) per link: {dark:?}");
    stack
        .impair_link_both_ways(0, ImpairmentConfig::default())
        .expect("restore link 0");

    let output = stack.stop();
    common::dump_output(&output);

    for i in 0..2 {
        assert_eq!(
            dark[i].2, clean[i].2,
            "link {i} reconnected between the snapshots, so their deltas mean nothing: {clean:?} \
             -> {dark:?}"
        );
    }
    assert_eq!(
        dark[0].1, clean[0].1,
        "a black-holed link gained arrival reports: {clean:?} -> {dark:?}"
    );
    assert!(
        dark[1].1 > clean[1].1 + 100,
        "the healthy link stopped gaining arrival reports: {clean:?} -> {dark:?}"
    );
    let stderr: String = output.srtla_send_stderr.join("\n");
    assert!(
        !stderr.contains("PANIC") && !stderr.contains("panic"),
        "srtla_send panicked"
    );
}
