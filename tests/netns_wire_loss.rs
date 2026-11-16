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

use std::collections::{BTreeMap, HashSet};
use std::path::Path;
use std::thread;
use std::time::Duration;

use network_sim::{ImpairmentConfig, NamespaceProcess};
use srtla_core::seq::seq_after;

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

    // Started before the SRT session, so the capture holds every data packet.
    let capture_dir = tempfile::tempdir().expect("create link capture dir");
    let captures = stack
        .start_link_captures(capture_dir.path())
        .inspect_err(|e| eprintln!("link capture skipped: {e:#}"))
        .ok();

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
    let nak_lines = nak_attributed_lines(&output.srtla_send_stdout);
    print_nak_attribution(&nak_lines);
    if let Some(captures) = captures {
        print_link_captures(
            capture_dir.path(),
            &stack.topo.sender_ips,
            captures,
            &nak_lines,
        );
    }
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

/// The link label of a stripped `nak attributed` line. It reads
/// `host:port via ip` and the host is the same on every link, so the label is
/// taken whole from `link=` up to ` seq=` rather than split on spaces.
fn nak_link_label(line: &str) -> Option<&str> {
    line.split_once("link=")
        .and_then(|(_, rest)| rest.split_once(" seq="))
        .map(|(link, _)| link)
}

/// Tallies stripped `nak attributed` lines per link. A line whose label cannot
/// be found lands under `?`, so a format change shows up in the summary instead
/// of silently emptying it.
fn tally_naks<'a>(lines: impl IntoIterator<Item = &'a str>) -> BTreeMap<String, NakTally> {
    let mut tallies: BTreeMap<String, NakTally> = BTreeMap::new();
    for line in lines {
        let link = nak_link_label(line).unwrap_or("?");
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

/// srtla_send's `nak attributed` lines, stripped of colour codes.
fn nak_attributed_lines(stdout: &[String]) -> Vec<String> {
    stdout
        .iter()
        .filter(|l| l.contains("nak attributed"))
        .map(|l| strip_ansi(l))
        .collect()
}

/// Prints the evidence for whether the clean link's NAKs are reorder NAKs.
/// It runs before the assertions so whichever one fails still has it in the
/// output.
fn print_nak_attribution(lines: &[String]) {
    eprintln!("--- nak attribution summary ---");
    for (link, t) in tally_naks(lines.iter().map(String::as_str)) {
        eprintln!(
            "  {link}: total={} resends={} early(age<rtt)={} late(age>=rtt)={}",
            t.total, t.resends, t.early, t.late
        );
    }

    const SHOWN: usize = 40;
    if lines.len() <= 2 * SHOWN {
        for l in lines {
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

/// What the captures at both ends of one link show. NAKs cannot tell a lost
/// packet from one that arrived after a higher sequence on the other link,
/// because the receiver reports a gap either way. The captures can.
#[derive(Debug, Default, PartialEq)]
struct LinkCaptureSummary {
    sent: usize,
    arrived: usize,
    /// Sent on this link and seen arriving on no link.
    lost: usize,
    /// Arrivals on this link after a strictly higher sequence had already
    /// arrived on any link.
    reordered: usize,
    /// For each reordered arrival, in arrival order, how long after the first
    /// higher sequence it arrived.
    late_us: Vec<u64>,
}

/// `sent[i]` and `received[i]` are the `(timestamp_us, seq)` lists captured at
/// the two ends of link `i`. All receive captures run in one namespace, so
/// their timestamps share a clock and can be merged.
fn summarize_link_captures(
    sent: &[Vec<(u64, u32)>],
    received: &[Vec<(u64, u32)>],
) -> Vec<LinkCaptureSummary> {
    let arrived_anywhere: HashSet<u32> = received.iter().flatten().map(|&(_, seq)| seq).collect();
    let mut summaries: Vec<LinkCaptureSummary> = sent
        .iter()
        .zip(received)
        .map(|(sent, received)| {
            let sent: HashSet<u32> = sent.iter().map(|&(_, seq)| seq).collect();
            let arrived: HashSet<u32> = received.iter().map(|&(_, seq)| seq).collect();
            LinkCaptureSummary {
                sent: sent.len(),
                arrived: arrived.len(),
                lost: sent
                    .iter()
                    .filter(|seq| !arrived_anywhere.contains(seq))
                    .count(),
                ..Default::default()
            }
        })
        .collect();

    let mut arrivals: Vec<(u64, u32, usize)> = received
        .iter()
        .enumerate()
        .flat_map(|(link, r)| r.iter().map(move |&(ts, seq)| (ts, seq, link)))
        .collect();
    arrivals.sort_by_key(|&(ts, _, _)| ts);

    // Every arrival that set a new highest sequence, with its time. The first
    // arrival above a late packet always set a new high, so a binary search
    // over these finds it.
    let mut record_highs: Vec<(u32, u64)> = Vec::new();
    for (ts, seq, link) in arrivals {
        let ahead = |high: u32| seq_after(high as i32, seq as i32);
        match record_highs.last() {
            Some(&(high, _)) if ahead(high) => {
                let first_higher = record_highs.partition_point(|&(high, _)| !ahead(high));
                summaries[link].reordered += 1;
                summaries[link]
                    .late_us
                    .push(ts - record_highs[first_higher].1);
            }
            Some(&(high, _)) if high == seq => {}
            _ => record_highs.push((seq, ts)),
        }
    }
    summaries
}

/// Link index and seq of every NAK charged to a first send, one entry per
/// `nak attributed` line. The link is the one whose sender IP ends the label.
/// Resends are left out because the receive captures leave out retransmits.
fn naked_first_sends<'a>(
    lines: impl IntoIterator<Item = &'a str>,
    sender_ips: &[String],
) -> Vec<(usize, u32)> {
    lines
        .into_iter()
        .filter(|line| field(line, "resend") == Some("false"))
        .filter_map(|line| {
            let (_, ip) = nak_link_label(line)?.rsplit_once(" via ")?;
            let link = sender_ips.iter().position(|s| s == ip)?;
            let seq = field(line, "seq")?.parse().ok()?;
            Some((link, seq))
        })
        .collect()
}

/// Where the NAKed first sends charged to one link actually arrived. This
/// needs only the receive captures, so unlike `lost` it also sees packets a
/// qdisc dropped on the sender veth before the send capture could.
#[derive(Debug, Default, PartialEq)]
struct NakedArrivals {
    total: usize,
    /// The charged link delivered it, so the NAK was a reorder, not a loss.
    arrived_on_link: usize,
    arrived_elsewhere: usize,
    never_arrived: usize,
}

fn classify_naked_first_sends(
    naks: &[(usize, u32)],
    received: &[Vec<(u64, u32)>],
) -> Vec<NakedArrivals> {
    let arrived: Vec<HashSet<u32>> = received
        .iter()
        .map(|r| r.iter().map(|&(_, seq)| seq).collect())
        .collect();
    let mut by_link: Vec<NakedArrivals> = received.iter().map(|_| Default::default()).collect();
    for &(link, seq) in naks {
        let Some(tally) = by_link.get_mut(link) else {
            continue;
        };
        tally.total += 1;
        if arrived[link].contains(&seq) {
            tally.arrived_on_link += 1;
        } else if arrived.iter().any(|a| a.contains(&seq)) {
            tally.arrived_elsewhere += 1;
        } else {
            tally.never_arrived += 1;
        }
    }
    by_link
}

/// Prints the per-link capture summary, or why there is none. Like the NAK
/// summary it runs before the assertions.
fn print_link_captures(
    dir: &Path,
    sender_ips: &[String],
    mut captures: Vec<NamespaceProcess>,
    nak_lines: &[String],
) {
    for capture in &mut captures {
        capture.kill();
    }
    thread::sleep(Duration::from_millis(500));

    eprintln!("--- link capture summary ---");
    eprintln!(
        "  note: lost cannot see qdisc drops on the sender veth, because the send capture taps \
         packets after tbf and netem"
    );
    // tcpdump reports on exit how many packets the kernel dropped before it
    // could read them. Those would show up as lost here.
    for capture in &captures {
        for line in capture.stderr_lines() {
            eprintln!("  tcpdump: {line}");
        }
    }

    let read = |name: String| -> Result<Vec<(u64, u32)>, String> {
        let bytes = std::fs::read(dir.join(&name)).map_err(|e| {
            if e.kind() == std::io::ErrorKind::PermissionDenied {
                format!("{name} is not readable by the test user: {e}")
            } else {
                format!("read {name}: {e}")
            }
        })?;
        network_sim::srt_data_seqs_from_pcap(&bytes).map_err(|e| format!("parse {name}: {e:#}"))
    };
    let mut sent = Vec::new();
    let mut received = Vec::new();
    for i in 0..sender_ips.len() {
        match (
            read(format!("send_{i}.pcap")),
            read(format!("recv_{i}.pcap")),
        ) {
            (Ok(s), Ok(r)) => {
                sent.push(s);
                received.push(r);
            }
            (Err(e), _) | (_, Err(e)) => {
                eprintln!("  link capture summary skipped: {e}");
                return;
            }
        }
    }

    let ms = |us: Option<&u64>| us.map_or(0.0, |&us| us as f64 / 1000.0);
    for (i, (link, ip)) in summarize_link_captures(&sent, &received)
        .into_iter()
        .zip(sender_ips)
        .enumerate()
    {
        let mut late = link.late_us;
        late.sort_unstable();
        eprintln!(
            "link {i} ({ip}): sent={} arrived={} lost={} reordered={} late_ms_median={:.1} \
             late_ms_max={:.1}",
            link.sent,
            link.arrived,
            link.lost,
            link.reordered,
            ms(late.get(late.len() / 2)),
            ms(late.last()),
        );
    }

    let naks = naked_first_sends(nak_lines.iter().map(String::as_str), sender_ips);
    for (i, (link, ip)) in classify_naked_first_sends(&naks, &received)
        .into_iter()
        .zip(sender_ips)
        .enumerate()
    {
        eprintln!(
            "naked first sends on link {i} ({ip}): total={} arrived_on_link={} \
             arrived_elsewhere={} never_arrived={}",
            link.total, link.arrived_on_link, link.arrived_elsewhere, link.never_arrived,
        );
    }
}

#[test]
fn link_captures_separate_loss_from_reorder() {
    // Starts two below the wrap, so the run crosses from 0x7fff_ffff to 0.
    let s = |k: u32| (0x7fff_fffe + k) & 0x7fff_ffff;
    let sent = vec![
        vec![(1, s(1)), (3, s(3)), (5, s(5))],
        vec![(2, s(2)), (4, s(4)), (6, s(6))],
    ];
    let received = vec![
        // s(5) never arrives, and s(6) shows up again as a duplicate.
        vec![(10, s(1)), (45, s(3)), (55, s(6))],
        // s(4) overtakes s(2) and s(3).
        vec![(20, s(4)), (30, s(2)), (50, s(6))],
    ];

    let summaries = summarize_link_captures(&sent, &received);

    assert_eq!(
        summaries,
        vec![
            LinkCaptureSummary {
                sent: 3,
                arrived: 3,
                lost: 1,
                reordered: 1,
                late_us: vec![25],
            },
            LinkCaptureSummary {
                sent: 3,
                arrived: 3,
                lost: 0,
                reordered: 1,
                late_us: vec![10],
            },
        ]
    );
}

/// A `nak attributed` line as srtla_send prints it, colour codes included.
fn nak_line(link: &str, seq: u32, age: &str, rtt: u64, resend: bool) -> String {
    format!(
        "2026-11-16T10:00:00.000000Z \u{1b}[34mDEBUG\u{1b}[0m nak attributed \
         \u{1b}[3mlink\u{1b}[0m\u{1b}[2m=\u{1b}[0m{link} \
         \u{1b}[3mseq\u{1b}[0m\u{1b}[2m=\u{1b}[0m{seq} \
         \u{1b}[3mage_ms\u{1b}[0m\u{1b}[2m=\u{1b}[0m{age} \
         \u{1b}[3mrtt_ms\u{1b}[0m\u{1b}[2m=\u{1b}[0m{rtt} \
         \u{1b}[3mresend\u{1b}[0m\u{1b}[2m=\u{1b}[0m{resend} \
         \u{1b}[3mvia\u{1b}[0m\u{1b}[2m=\u{1b}[0m\"tracker\""
    )
}

#[test]
fn naked_first_sends_are_classified_by_where_they_arrived() {
    let sender_ips = ["10.10.1.1".to_string(), "10.10.2.1".to_string()];
    let lossy = "10.99.0.1:5000 via 10.10.1.1";
    let clean = "10.99.0.1:5000 via 10.10.2.1";
    let raw = [
        nak_line(clean, 7, "Some(12)", 60, false),
        nak_line(clean, 8, "Some(14)", 60, false),
        nak_line(clean, 9, "Some(70)", 60, false),
        nak_line(clean, 10, "None", 60, true),
        nak_line(lossy, 11, "Some(80)", 60, false),
        nak_line("10.99.0.1:5000 via 10.10.9.1", 12, "Some(80)", 60, false),
    ];
    let lines = nak_attributed_lines(&raw);
    let received = vec![vec![(5, 8), (6, 10)], vec![(1, 7), (2, 11)]];

    let naks = naked_first_sends(lines.iter().map(String::as_str), &sender_ips);
    let arrivals = classify_naked_first_sends(&naks, &received);

    // The resend and the line from an unknown link are left out.
    assert_eq!(naks, vec![(1, 7), (1, 8), (1, 9), (0, 11)]);
    assert_eq!(
        arrivals,
        vec![
            NakedArrivals {
                total: 1,
                arrived_on_link: 0,
                arrived_elsewhere: 1,
                never_arrived: 0,
            },
            NakedArrivals {
                total: 3,
                arrived_on_link: 1,
                arrived_elsewhere: 1,
                never_arrived: 1,
            },
        ]
    );
}

#[test]
fn nak_lines_are_tallied_per_link() {
    let line = |link: &str, age: &str, rtt: u64, resend: bool| nak_line(link, 42, age, rtt, resend);
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
