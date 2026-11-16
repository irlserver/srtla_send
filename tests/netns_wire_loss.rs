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
//!
//! Two links, both live, so this exercises real bonding rather than one
//! link with dead spares. Offered load comes from the *adaptive* SRT
//! sender, not a constant pump: it lowers its bitrate when the SRT send
//! buffer backs up or RTT inflates (belacoder's congestion response,
//! minus the encoder), so the rate tracks what the bond can carry rather
//! than oversubscribing it into an immediate retransmit-fuelled collapse.
//!
//! It keeps the run far healthier than a constant pump, but it does not
//! make it pristine: a real SRT session over a hard-capped lossy link
//! still oscillates, because settling at a clean steady rate would take
//! production-grade congestion control tuned for the path. So the checks
//! are the ones that hold *through* that oscillation: the lossy link's CC
//! target never ratchets to the floor (the fix), and both links carry a
//! real share at once (the bond is bonding). Aggregate goodput is logged
//! but not asserted — it is not a stable enough number to threshold on.

mod common;

use std::collections::BTreeMap;
use std::thread;
use std::time::Duration;

use network_sim::ImpairmentConfig;

/// Both links are the same generous size. Each is far above the share it
/// ends up carrying, so the lossy link stays uncongested and its 2% loss
/// is genuinely wire loss, not congestion — which is what the fix is
/// about. The adaptive sender ramps toward the bond's real capacity, so
/// both links get used without anyone having to guess an offered rate.
const LOSSY_LINK_KBIT: u64 = 6_000;
const CLEAN_LINK_KBIT: u64 = 6_000;

/// The adaptive sender's bitrate bounds. The ceiling sits above the
/// 12 Mbps the bond could carry, so the sender is free to ramp up until
/// the send buffer tells it to stop rather than being capped short.
const SENDER_MIN_KBPS: u32 = 500;
const SENDER_MAX_KBPS: u32 = 16_000;

const RUN_SECS: u64 = 45;

/// `MIN_TARGET_BPS` in link_cc. A link pinned here has a BDP in-flight
/// cap of about one packet and is effectively out of the bond.
const CC_FLOOR_BPS: u64 = 100_000;

#[test]
fn wire_loss_link_is_not_ratcheted_out_of_the_bond() {
    if common::skip_without_impairment_deps() {
        return;
    }
    // The adaptive sender is compiled here against the system libsrt.
    if let Err(reason) = network_sim::check_adaptive_sender_deps()
        && common::skip_or_fail(reason)
    {
        return;
    }
    let sender_bin = network_sim::build_adaptive_sender().expect("build adaptive SRT sender");

    let sock = format!("/tmp/srtla-wireloss-{}.sock", std::process::id());
    let mut stack =
        common::start_stack("wireloss", 2, &["--control-socket", &sock]).expect("start stack");

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
    stack
        .start_adaptive_sender(&sender_bin, SENDER_MIN_KBPS, SENDER_MAX_KBPS)
        .expect("start adaptive SRT sender");

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

    // The adaptive sender lives in the stack's caller slot, so stop()
    // kills it along with everything else.
    let output = stack.stop();
    print_nak_attribution(&output.srtla_send_stdout);
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
        eprintln!("--- last 80 lines of srtla_send stdout (RUST_LOG=debug) ---");
        for l in output.srtla_send_stdout.iter().rev().take(80).rev() {
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
         bps, lossy CC target low-water={lossy_target_min} bps"
    );

    // 1. THE fix. The lossy link's CC target must never ratchet to the
    //    floor. Before the load gate, the efficacy test, and the
    //    delivered floor, BackingOff compounded -15% every tick for as
    //    long as the loss lasted, pinning the target at MIN_TARGET_BPS in
    //    ~20s. Holds on every run so far, never near the floor.
    assert!(
        lossy_target_min > CC_FLOOR_BPS * 3,
        "lossy link's CC target collapsed to {lossy_target_min} bps (floor is {CC_FLOOR_BPS}) — \
         wire loss ratcheted a healthy {LOSSY_LINK_KBIT} kbit link out of the bond"
    );

    // 2. Both links carry a real share at once — this is a bond, not a
    //    failover, and getting the second link to register and pull
    //    traffic is the point of the topology work in the harness.
    //
    //    A modest per-link floor, not a goodput target. The adaptive
    //    sender keeps the run far healthier than the old constant pump
    //    (which collapsed almost immediately), but an SRT session over a
    //    hard-capped lossy link still oscillates — it does not settle at
    //    a clean steady rate. Reaching that would take production-grade
    //    congestion control (belacoder's, tuned for real cellular), which
    //    is well beyond what this test needs. So assert liveness, which
    //    holds through the oscillation, not a throughput figure that
    //    would be flaky.
    let each_link_floor = 500_000; // 0.5 Mbps: clearly pulling weight, not idle
    assert!(
        lossy_median > each_link_floor,
        "lossy link carried only {lossy_median} bps — it is nominally in the bond but effectively \
         idle"
    );
    assert!(
        clean_median > each_link_floor,
        "clean link carried only {clean_median} bps — the bond is really running on one link"
    );

    // bond_median is logged above for the operator; not asserted, since
    // under oscillation it is not a stable aggregate to threshold on.
    let _ = bond_median;
}

/// How one link's attributed NAKs split up. An early NAK names a packet sent
/// less than one RTT before it, which the receiver could not have detected as
/// lost and reported back in time. That is the fingerprint of a reorder NAK on
/// a bond, where a later sequence arrives over the other link first.
#[derive(Debug, Default, PartialEq)]
struct NakTally {
    total: usize,
    resends: usize,
    early: usize,
    late: usize,
}

/// The captured stdout keeps tracing's colour codes, which wrap each field
/// name and its `=`, so field lookup only works on the stripped line.
fn strip_ansi(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            for c in chars.by_ref() {
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn field<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    line.split_whitespace()
        .find_map(|token| token.strip_prefix(key)?.strip_prefix('='))
}

/// Tallies stripped `nak attributed` lines per link. The link label reads
/// `host:port via ip` and the host is the same on every link, so the label is
/// taken whole from `link=` up to ` seq=` rather than split on spaces. A line
/// whose label cannot be found lands under `?`, so a format change shows up in
/// the summary instead of silently emptying it.
fn tally_naks<'a>(lines: impl IntoIterator<Item = &'a str>) -> BTreeMap<String, NakTally> {
    let mut tallies: BTreeMap<String, NakTally> = BTreeMap::new();
    for line in lines {
        let link = line
            .split_once("link=")
            .and_then(|(_, rest)| rest.split_once(" seq="))
            .map_or("?", |(link, _)| link);
        let tally = tallies.entry(link.to_string()).or_default();
        tally.total += 1;
        if field(line, "resend") == Some("true") {
            tally.resends += 1;
            continue;
        }
        let age = field(line, "age_ms").and_then(|v| {
            v.strip_prefix("Some(")?
                .strip_suffix(')')?
                .parse::<u64>()
                .ok()
        });
        let rtt = field(line, "rtt_ms").and_then(|v| v.parse::<u64>().ok());
        if let (Some(age), Some(rtt)) = (age, rtt) {
            if age < rtt {
                tally.early += 1;
            } else {
                tally.late += 1;
            }
        }
    }
    tallies
}

/// Prints the evidence for whether the clean link's NAKs are reorder NAKs.
/// It runs before the assertions so whichever one fails still has it in the
/// output.
fn print_nak_attribution(stdout: &[String]) {
    let lines: Vec<String> = stdout
        .iter()
        .filter(|l| l.contains("nak attributed"))
        .map(|l| strip_ansi(l))
        .collect();

    eprintln!("--- nak attribution summary ---");
    for (link, t) in tally_naks(lines.iter().map(String::as_str)) {
        eprintln!(
            "  {link}: total={} resends={} early(age<rtt)={} late(age>=rtt)={}",
            t.total, t.resends, t.early, t.late
        );
    }

    const SHOWN: usize = 40;
    if lines.len() <= 2 * SHOWN {
        for l in &lines {
            eprintln!("{l}");
        }
    } else {
        for l in &lines[..SHOWN] {
            eprintln!("{l}");
        }
        eprintln!(
            "... {} more nak attributed lines ...",
            lines.len() - 2 * SHOWN
        );
        for l in &lines[lines.len() - SHOWN..] {
            eprintln!("{l}");
        }
    }
}

#[test]
fn nak_lines_are_tallied_per_link() {
    let line = |link: &str, age: &str, rtt: u64, resend: bool| {
        format!(
            "2026-11-16T10:00:00.000000Z \u{1b}[34mDEBUG\u{1b}[0m nak attributed \
             \u{1b}[3mlink\u{1b}[0m\u{1b}[2m=\u{1b}[0m{link} \
             \u{1b}[3mseq\u{1b}[0m\u{1b}[2m=\u{1b}[0m42 \
             \u{1b}[3mage_ms\u{1b}[0m\u{1b}[2m=\u{1b}[0m{age} \
             \u{1b}[3mrtt_ms\u{1b}[0m\u{1b}[2m=\u{1b}[0m{rtt} \
             \u{1b}[3mresend\u{1b}[0m\u{1b}[2m=\u{1b}[0m{resend} \
             \u{1b}[3mvia\u{1b}[0m\u{1b}[2m=\u{1b}[0m\"tracker\""
        )
    };
    let lossy = "10.0.0.1:5000 via 10.1.0.2";
    let clean = "10.0.0.1:5000 via 10.2.0.2";
    let raw = [
        line(lossy, "Some(80)", 60, false),
        line(clean, "Some(12)", 60, false),
        line(clean, "Some(30)", 60, false),
        line(clean, "None", 60, true),
        line(clean, "Some(60)", 60, false),
    ];
    let stripped: Vec<String> = raw.iter().map(|l| strip_ansi(l)).collect();

    let tallies = tally_naks(stripped.iter().map(String::as_str));

    assert_eq!(tallies.len(), 2);
    assert_eq!(
        tallies[lossy],
        NakTally {
            total: 1,
            resends: 0,
            early: 0,
            late: 1,
        }
    );
    assert_eq!(
        tallies[clean],
        NakTally {
            total: 4,
            resends: 1,
            early: 2,
            late: 1,
        }
    );
}
