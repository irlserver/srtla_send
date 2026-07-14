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

/// The lossy link is *roomy*: 8 Mbps, far above the ~2.5 Mbps it ends up
/// carrying. Nothing we send can congest it, so its 2% loss is wire loss
/// by construction — which is the whole point of the scenario.
const LOSSY_LINK_KBIT: u64 = 8_000;

/// The clean link is deliberately too small to carry the stream alone.
///
/// This is what the first version of this test got wrong: with both
/// links at 8 Mbps and only 2 Mbps offered, the clean link could swallow
/// the entire stream, so the scheduler never had any reason to pick the
/// lossy one. Link 0 sat at zero throughput and the congestion-control
/// path under test was never executed. Starving the clean link forces
/// the bond to actually use the lossy one.
const CLEAN_LINK_KBIT: u64 = 1_500;

/// ~4 Mbps offered: comfortably more than the clean link's 1.5 Mbps, so
/// roughly 2.5 Mbps has to go down the lossy link, which is still well
/// under its 8 Mbps ceiling.
const PACKETS_PER_SEC: u32 = 380;
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
    eprintln!(
        "\nsteady state: bond={bond_median} bps, lossy link={lossy_median} bps, \
         offered={OFFERED_BPS} bps, lossy CC target low-water={lossy_target_min} bps"
    );

    // 1. The CC must not have ratcheted the lossy link's cap into the
    //    ground. Before the load gate and the efficacy test, BackingOff
    //    compounded -15% every tick for as long as the loss lasted,
    //    which pinned the target at MIN_TARGET_BPS within ~20s.
    assert!(
        lossy_target_min > CC_FLOOR_BPS * 3,
        "lossy link's CC target collapsed to {lossy_target_min} bps (floor is {CC_FLOOR_BPS}) — \
         steady wire loss ratcheted a healthy 8 Mbps link out of the bond"
    );

    // 2. And it must still have been *carrying* traffic. A target that
    //    stays high while the link sits idle would satisfy (1) without
    //    the bond gaining anything, so assert the delivered rate too.
    //    The clean link alone caps out at 1.5 Mbps.
    assert!(
        lossy_median > CLEAN_LINK_KBIT * 1_000 / 2,
        "lossy link only carried {lossy_median} bps in steady state — it is nominally in the bond \
         but is not doing real work"
    );

    // 3. The point of all of it: the bond aggregates. Losing the lossy
    //    link would cap the bond at the clean link's 1.5 Mbps.
    assert!(
        bond_median > CLEAN_LINK_KBIT * 1_000 * 3 / 2,
        "bond carried only {bond_median} bps — barely more than the clean link's \
         {CLEAN_LINK_KBIT} kbit on its own, so bonding gained nothing"
    );
}
