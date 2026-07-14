//! Does a link with steady wire loss survive in the bond?
//!
//! The per-link CC soft cap (`sender::selection::link_cc`) reacts to NAK
//! loss by cutting `target_bps`. Loss that our own offered rate did not
//! cause cannot be repaired by cutting, so a link with a percent or two
//! of steady radio loss must not be driven out of the bond by it.
//!
//! This is the end-to-end counterpart to the unit tests in `link_cc`,
//! and it exists because those tests can only check the controller
//! against a *model* of wire loss that we wrote ourselves. Here the loss
//! is real (`tc netem`), the NAKs are real (a genuine SRT session runs
//! through the bond), and the CC reads them through the production path.
//!
//! Note the stack deliberately runs a real SRT caller in front of
//! srtla_send. Injecting raw UDP — as the older impairment tests do —
//! never completes an SRT handshake at the far end, so no ACKs or NAKs
//! ever come back and the entire congestion-control path is dead code
//! under test.

mod common;

use std::thread;
use std::time::Duration;

use network_sim::{ImpairmentConfig, SRT_CALLER_INGEST_PORT, SRT_PAYLOAD_BYTES, SrtlaTestStack};

/// Both links are the same generous size. The scenario has to hold two
/// things at once, and they pull in opposite directions:
///
/// - The lossy link must be *uncongested*, or its loss stops being wire
///   loss and the test no longer isolates the bug. So each link is far
///   larger than its share of the offered stream.
/// - Both links must actually be *used*, or there is no bond to speak
///   of. So the offered rate exceeds what either link carries alone,
///   forcing the scheduler to spread across both.
///
/// 6 Mbps links. With two equal links there is an unavoidable squeeze:
/// "use both" needs the offered rate above one link (>6 Mbps), i.e. above
/// half the 12 Mbps total, so the bond always runs hot. Push too far past
/// that and SRT's retransmits amplify the loss into congestion collapse.
/// So keep the offered rate only a little over one link.
const LOSSY_LINK_KBIT: u64 = 6_000;
const CLEAN_LINK_KBIT: u64 = 6_000;

/// ~7 Mbps offered as 1316-byte datagrams: over one link's 6 Mbps so the
/// bond must use both, but only ~58% of the 12 Mbps total, which leaves
/// enough headroom to stay out of collapse. (The other half of avoiding
/// collapse is the SRT caller's 2s latency exceeding the shaper buffer —
/// see `start_srt_caller`.)
const PACKETS_PER_SEC: u32 = 665;
const RUN_SECS: u64 = 45;

/// Bits actually offered per second, for reference in assertions.
const OFFERED_BPS: u64 = PACKETS_PER_SEC as u64 * SRT_PAYLOAD_BYTES as u64 * 8;

/// `MIN_TARGET_BPS` in link_cc. A link pinned here has a BDP in-flight
/// cap of about one packet and is effectively out of the bond.
const CC_FLOOR_BPS: u64 = 100_000;

#[test]
fn wire_loss_link_is_not_ratcheted_out_of_the_bond() {
    if common::skip_without_impairment_deps() {
        return;
    }
    common::build_srtla_send();

    let sock = format!("/tmp/srtla-wireloss-{}.sock", std::process::id());
    let mut stack =
        SrtlaTestStack::start("wireloss", 2, &["--control-socket", &sock]).expect("start stack");

    // Same delay on both, so RTT cannot be what separates them: the only
    // difference the scheduler can see is loss.
    stack
        .impair_link(
            0,
            ImpairmentConfig {
                delay_ms: Some(25),
                loss_percent: Some(2.0),
                rate_kbit: Some(LOSSY_LINK_KBIT),
                tbf_shaping: true,
                ..Default::default()
            },
        )
        .expect("impair link 0");
    stack
        .impair_link(
            1,
            ImpairmentConfig {
                delay_ms: Some(25),
                rate_kbit: Some(CLEAN_LINK_KBIT),
                tbf_shaping: true,
                ..Default::default()
            },
        )
        .expect("impair link 1");

    common::wait_until_ready(&stack);
    stack.start_srt_caller().expect("start srt caller");

    // Feed the SRT caller for the whole run while we sample the CC.
    let mut pump = network_sim::spawn_udp_stream(
        &stack.topo.sender_ns,
        "127.0.0.1",
        SRT_CALLER_INGEST_PORT,
        PACKETS_PER_SEC,
        SRT_PAYLOAD_BYTES,
        Duration::from_secs(RUN_SECS),
    )
    .expect("spawn traffic pump");

    let mut lossy_target_min = u64::MAX;
    let mut samples = 0usize;
    let mut total_ticks = 0usize;
    let mut saw_naks = false;
    let mut saw_any_traffic = false;
    let mut bond_bps_steady: Vec<u64> = Vec::new();
    let mut lossy_bps_steady: Vec<u64> = Vec::new();
    let mut clean_bps_steady: Vec<u64> = Vec::new();
    let mut prev_line = String::new();
    let mut frozen_ticks = 0usize;

    for _ in 0..RUN_SECS {
        thread::sleep(Duration::from_secs(1));
        let Ok(stats) = stack.get_stats(&sock) else {
            continue;
        };
        let Some(links) = stats.get("links").and_then(|l| l.as_array()) else {
            continue;
        };
        if links.len() < 2 {
            continue;
        }
        // Print every link, not just the lossy one. Reading link 0 alone
        // cannot distinguish "the bond carried nothing" from "the
        // scheduler sent it all down link 1" — and those call for
        // completely different fixes.
        // `bitrate_bytes_per_sec` is bytes, `cc_target_bps` is bits.
        // Normalise to bits here so the two are actually comparable.
        let link_bps = |l: &serde_json::Value| {
            l.get("bitrate_bytes_per_sec")
                .and_then(|v| v.as_u64())
                .unwrap_or(0)
                * 8
        };

        let mut line = String::new();
        for (i, l) in links.iter().enumerate() {
            let f = |k: &str| l.get(k).and_then(|v| v.as_u64()).unwrap_or(0);
            let state = l.get("cc_state").and_then(|v| v.as_str()).unwrap_or("?");
            let naks = l.get("nak_count").and_then(|v| v.as_i64()).unwrap_or(0);
            line.push_str(&format!(
                "  [{i}{}] {state:<11} target={:<9} sent_bps={:<9} window={:<6} inflight={:<4} \
                 naks={naks}\n",
                if i == 0 { "*lossy" } else { "      " },
                f("cc_target_bps"),
                link_bps(l),
                f("window"),
                f("in_flight"),
            ));
        }
        eprintln!("t+{total_ticks}s\n{line}");
        total_ticks += 1;

        if line == prev_line {
            frozen_ticks += 1;
        } else {
            frozen_ticks = 0;
        }
        prev_line = line;

        let lossy = &links[0];
        let target = lossy
            .get("cc_target_bps")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let naks = lossy.get("nak_count").and_then(|v| v.as_i64()).unwrap_or(0);
        let state = lossy
            .get("cc_state")
            .and_then(|v| v.as_str())
            .unwrap_or("?");
        let bond_bps: u64 = links.iter().map(link_bps).sum();

        if bond_bps > 0 {
            saw_any_traffic = true;
        }
        // Only judge steady state: give registration, the SRT handshake,
        // and the CC seed time to settle before scoring throughput.
        if total_ticks > 10 {
            bond_bps_steady.push(bond_bps);
            lossy_bps_steady.push(link_bps(lossy));
            clean_bps_steady.push(link_bps(&links[1]));
        }
        // Ignore the bootstrap ticks: the target is parked at the floor
        // until the first RTT sample, which would trivially satisfy the
        // assertion below in the wrong direction.
        if state == "bootstrap" {
            continue;
        }
        if naks > 0 {
            saw_naks = true;
        }
        lossy_target_min = lossy_target_min.min(target);
        samples += 1;
    }

    pump.kill();
    let output = stack.stop();
    let _ = std::fs::remove_file(&sock);

    // srtla_send runs housekeeping, the weak-link classifier and the CC
    // controller in one tokio task, and writes the stats snapshot at the
    // end of it. A panic anywhere in there kills only that task: the
    // control socket lives in a different task and keeps happily serving
    // the last snapshot it saw. The symptom is a stats reply that is
    // byte-identical forever, which reads like a suspiciously stable
    // control loop rather than a dead one. Say so explicitly.
    let panics: Vec<&String> = output
        .srtla_send_stderr
        .iter()
        .filter(|l| l.contains("panicked at") || l.contains("PANIC"))
        .collect();
    assert!(
        panics.is_empty(),
        "srtla_send panicked — housekeeping/CC task is dead, so every stat below is stale:\n{}",
        panics
            .iter()
            .map(|l| format!("  {l}"))
            .collect::<Vec<_>>()
            .join("\n")
    );

    // Even without a panic message, a snapshot that never changes while
    // traffic is flowing means the loop that produces it is not running.
    if frozen_ticks > 10 {
        eprintln!("--- last 80 lines of srtla_send stderr (RUST_LOG=debug) ---");
        for l in output.srtla_send_stderr.iter().rev().take(80).rev() {
            eprintln!("{l}");
        }
        panic!(
            "srtla_send's stats snapshot did not change for {frozen_ticks} consecutive seconds \
             while traffic was flowing — the housekeeping/CC task has stopped. Nothing below this \
             point is a measurement of congestion control."
        );
    }

    assert!(
        samples > 10,
        "too few post-bootstrap CC samples ({samples})"
    );

    // Nothing crossed the bond at all: the SRT session never carried
    // data, so this is a broken harness, not a result about the CC.
    if !saw_any_traffic {
        eprintln!("--- srt caller stderr ---");
        for l in &output.srt_caller_stderr {
            eprintln!("{l}");
        }
        eprintln!("--- srt listener stderr ---");
        for l in &output.srt_server_stderr {
            eprintln!("{l}");
        }
        panic!(
            "no data crossed the bond on any link — the SRT session never established, so this \
             test is not exercising congestion control at all"
        );
    }

    // Traffic flowed, but never down the lossy link. That is a real
    // finding rather than a harness bug — it means link *scoring* sheds
    // the lossy link before the CC ever sees it — but it still leaves
    // the CC path untested, so it must not be reported as a pass.
    assert!(
        saw_naks,
        "traffic crossed the bond but the lossy link never took any, so it never NAKed. The \
         scheduler is starving it on quality score before congestion control is reached — the CC \
         path under test is never executed"
    );

    let median = |mut v: Vec<u64>| -> u64 {
        if v.is_empty() {
            return 0;
        }
        v.sort_unstable();
        v[v.len() / 2]
    };
    let bond_median = median(bond_bps_steady);
    let lossy_median = median(lossy_bps_steady.clone());
    let clean_median = median(clean_bps_steady);
    eprintln!(
        "\nsteady state: bond={bond_median} bps, lossy={lossy_median} bps, clean={clean_median} \
         bps, offered={OFFERED_BPS} bps, lossy CC target low-water={lossy_target_min} bps"
    );

    // 1. The CC must not have ratcheted the lossy link's cap into the
    //    ground. Before the load gate, the efficacy test, and the
    //    delivered floor, BackingOff compounded -15% every tick for as
    //    long as the loss lasted, pinning the target at MIN_TARGET_BPS.
    assert!(
        lossy_target_min > CC_FLOOR_BPS * 3,
        "lossy link's CC target collapsed to {lossy_target_min} bps (floor is {CC_FLOOR_BPS}) — \
         steady wire loss ratcheted a healthy {LOSSY_LINK_KBIT} kbit link out of the bond"
    );

    // 2. Both links must be carrying real traffic at once — that is what
    //    makes this a bond rather than a failover. A quarter of the
    //    offered rate on each is a generous floor (fair share is ~half)
    //    that still fails loudly if either link is idle or trickling.
    let each_link_floor = OFFERED_BPS / 4;
    assert!(
        lossy_median > each_link_floor,
        "lossy link carried only {lossy_median} bps of {OFFERED_BPS} offered — it is nominally in \
         the bond but not pulling its weight"
    );
    assert!(
        clean_median > each_link_floor,
        "clean link carried only {clean_median} bps of {OFFERED_BPS} offered — the bond is really \
         running on one link"
    );

    // 3. The bond aggregates past what either link can do alone. Each is
    //    capped at 6 Mbps, so clearing that ceiling proves both links are
    //    summing rather than one covering for the other.
    let single_link_bps = LOSSY_LINK_KBIT.max(CLEAN_LINK_KBIT) * 1_000;
    assert!(
        bond_median > single_link_bps,
        "bond carried only {bond_median} bps — no more than a single {single_link_bps}-bps link, \
         so bonding gained nothing"
    );
}
