//! SIGHUP IP-list reload guard.
//!
//! Mirrors the C sender's reload guard (`srtla/src/sender_logic.h`,
//! `count_parseable_source_ips` / `analyze_reload_error`): a SIGHUP reload that
//! resolves to zero usable source IPs — a missing/unreadable, empty, or
//! all-garbage file — is REFUSED so the stream keeps running on the existing
//! links instead of tearing every connection down. A file mixing valid and
//! invalid lines still applies; the bad lines are skipped with a warning.
//!
//! Each line is `<ip>[ <weight>]`. The optional weight is an operator link
//! weight (Moblin's "connection priorities"): an integer 1..10, missing = 1,
//! larger values clamped to 10, and a `0` or unparsable weight is warned about
//! and read as 1 — the IP on that line still counts, so a weight can never make
//! the reload guard refuse a file it used to accept. Weights are normalised so
//! the lowest one is 1 on every load (see
//! [`srtla_core::connection::normalise_link_weights`]).

use std::net::IpAddr;
use std::str::FromStr;

use smallvec::SmallVec;
use srtla_core::connection::{LINK_WEIGHT_MAX, LINK_WEIGHT_MIN, normalise_link_weights};
use tracing::warn;

/// Why a SIGHUP reload was refused. In every case the existing connections are
/// kept and the stream keeps running.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReloadRefusal {
    /// The ips file could not be opened or read. Only reachable from
    /// [`analyze_ip_reload`], which is the SIGHUP entry point and therefore
    /// unix-only.
    #[cfg(unix)]
    NotFound,
    /// The ips file has no non-blank lines.
    Empty,
    /// The ips file has content but no line parses as an IP. Carries the 1-based
    /// line number of the first invalid line for operator-facing logging.
    NoValidIps { first_invalid_line: usize },
}

/// Outcome of analyzing an ips file for a SIGHUP reload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IpReload {
    /// Apply this (guaranteed non-empty) IP list. `first_invalid_line` is
    /// `Some(n)` when at least one line was skipped as invalid (a mixed
    /// valid+invalid file), otherwise `None`.
    Apply {
        ips: SmallVec<IpAddr, 4>,
        /// Normalised operator weight per entry of `ips` (same order, same
        /// length). All 1 when the file carries no weights.
        weights: SmallVec<u8, 4>,
        first_invalid_line: Option<usize>,
    },
    /// Refuse the reload and keep the current connections.
    Refuse(ReloadRefusal),
}

/// Analyze ips-file `text` for a SIGHUP reload, applying the same
/// zero-valid-IP guard as the C sender. Pure and synchronous so it is
/// unit-testable without touching the filesystem; [`analyze_ip_reload`] layers
/// the file read on top.
pub fn analyze_ip_reload_text(text: &str) -> IpReload {
    let mut ips: SmallVec<IpAddr, 4> = SmallVec::new();
    let mut weights: SmallVec<u8, 4> = SmallVec::new();
    let mut first_invalid_line: Option<usize> = None;
    let mut saw_content = false;

    for (idx, line) in text.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        saw_content = true;
        match parse_ip_line(trimmed, idx + 1) {
            Some((ip, weight)) => {
                ips.push(ip);
                weights.push(weight);
            }
            None => {
                if first_invalid_line.is_none() {
                    first_invalid_line = Some(idx + 1);
                }
            }
        }
    }
    normalise_link_weights(&mut weights);

    if ips.is_empty() {
        return if saw_content {
            IpReload::Refuse(ReloadRefusal::NoValidIps {
                first_invalid_line: first_invalid_line.unwrap_or(1),
            })
        } else {
            IpReload::Refuse(ReloadRefusal::Empty)
        };
    }

    IpReload::Apply {
        ips,
        weights,
        first_invalid_line,
    }
}

/// Parse one non-blank line as `<ip>[ <weight>]`. `None` = the line has no
/// valid IP (or more than two fields) and is skipped like any invalid line. A
/// bad weight never invalidates the line: it is warned about and read as 1.
fn parse_ip_line(line: &str, line_no: usize) -> Option<(IpAddr, u8)> {
    let mut fields = line.split_whitespace();
    let ip = IpAddr::from_str(fields.next()?).ok()?;
    let weight = match fields.next() {
        None => LINK_WEIGHT_MIN,
        Some(raw) => match raw.parse::<u64>() {
            Ok(0) | Err(_) => {
                warn!(
                    "ips file line {line_no}: weight {raw:?} for {ip} is not an integer \
                     {LINK_WEIGHT_MIN}..{LINK_WEIGHT_MAX}; using {LINK_WEIGHT_MIN}"
                );
                LINK_WEIGHT_MIN
            }
            Ok(w) if w > u64::from(LINK_WEIGHT_MAX) => {
                warn!(
                    "ips file line {line_no}: weight {w} for {ip} is above {LINK_WEIGHT_MAX}; \
                     clamping to {LINK_WEIGHT_MAX}"
                );
                LINK_WEIGHT_MAX
            }
            Ok(w) => w as u8,
        },
    };
    if fields.next().is_some() {
        return None;
    }
    Some((ip, weight))
}

/// Read `path` and analyze it for a SIGHUP reload. A read error maps to
/// [`ReloadRefusal::NotFound`] — the C guard treats an unreadable file as zero
/// valid IPs and refuses the reload.
///
/// Unix-only: reload is driven by SIGHUP, which Windows does not have. Startup
/// parsing goes through [`analyze_ip_reload_text`] on every platform.
#[cfg(unix)]
pub fn analyze_ip_reload(path: &str) -> IpReload {
    match std::fs::read_to_string(path) {
        Ok(text) => analyze_ip_reload_text(&text),
        Err(_) => IpReload::Refuse(ReloadRefusal::NotFound),
    }
}

#[cfg(test)]
mod tests {
    // Only the two file-reading tests exercise the SIGHUP-only
    // `analyze_ip_reload`; they and their imports are unix-gated like it,
    // so `cargo test` still compiles on Windows.
    #[cfg(unix)]
    use std::io::Write;
    #[cfg(unix)]
    use std::net::Ipv4Addr;

    #[cfg(unix)]
    use tempfile::NamedTempFile;

    use super::*;

    fn ip(s: &str) -> IpAddr {
        IpAddr::from_str(s).unwrap()
    }

    #[test]
    fn all_valid_applies_without_invalid_line() {
        match analyze_ip_reload_text("10.0.0.1\n10.0.0.2\n") {
            IpReload::Apply {
                ips,
                first_invalid_line,
                ..
            } => {
                assert_eq!(ips.as_slice(), [ip("10.0.0.1"), ip("10.0.0.2")]);
                assert_eq!(first_invalid_line, None);
            }
            other => panic!("expected Apply, got {other:?}"),
        }
    }

    #[test]
    fn blank_lines_are_skipped_not_counted_as_invalid() {
        match analyze_ip_reload_text("\n10.0.0.1\n   \n10.0.0.2\n\n") {
            IpReload::Apply {
                ips,
                first_invalid_line,
                ..
            } => {
                assert_eq!(ips.as_slice(), [ip("10.0.0.1"), ip("10.0.0.2")]);
                assert_eq!(first_invalid_line, None);
            }
            other => panic!("expected Apply, got {other:?}"),
        }
    }

    #[test]
    fn mixed_valid_and_invalid_applies_and_reports_first_invalid_line() {
        // Line 2 is the first invalid line; the valid IPs still apply.
        match analyze_ip_reload_text("10.0.0.1\nnot-an-ip\n10.0.0.2\nalso-bad\n") {
            IpReload::Apply {
                ips,
                first_invalid_line,
                ..
            } => {
                assert_eq!(ips.as_slice(), [ip("10.0.0.1"), ip("10.0.0.2")]);
                assert_eq!(first_invalid_line, Some(2));
            }
            other => panic!("expected Apply, got {other:?}"),
        }
    }

    #[test]
    fn all_garbage_refuses_with_first_invalid_line() {
        assert_eq!(
            analyze_ip_reload_text("garbage\nstill-not-an-ip\n"),
            IpReload::Refuse(ReloadRefusal::NoValidIps {
                first_invalid_line: 1,
            })
        );
    }

    #[test]
    fn garbage_after_blanks_reports_correct_line_number() {
        // Line 3 holds the first (and only) non-blank, invalid entry.
        assert_eq!(
            analyze_ip_reload_text("\n\n###garbage###\n"),
            IpReload::Refuse(ReloadRefusal::NoValidIps {
                first_invalid_line: 3,
            })
        );
    }

    #[test]
    fn empty_file_refuses_as_empty() {
        assert_eq!(
            analyze_ip_reload_text(""),
            IpReload::Refuse(ReloadRefusal::Empty)
        );
    }

    #[test]
    fn only_blank_lines_refuses_as_empty() {
        assert_eq!(
            analyze_ip_reload_text("\n   \n\t\n"),
            IpReload::Refuse(ReloadRefusal::Empty)
        );
    }

    #[cfg(unix)]
    #[test]
    fn missing_file_refuses_as_not_found() {
        assert_eq!(
            analyze_ip_reload("/nonexistent/srtla-reload-guard-test.txt"),
            IpReload::Refuse(ReloadRefusal::NotFound)
        );
    }

    #[cfg(unix)]
    #[test]
    fn reads_and_parses_a_real_file() {
        let mut f = NamedTempFile::new().unwrap();
        writeln!(f, "127.0.0.1").unwrap();
        writeln!(f, "127.0.0.2").unwrap();
        f.flush().unwrap();
        match analyze_ip_reload(f.path().to_str().unwrap()) {
            IpReload::Apply { ips, .. } => {
                assert_eq!(
                    ips.as_slice(),
                    [
                        IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)),
                        IpAddr::V4(Ipv4Addr::new(127, 0, 0, 2)),
                    ]
                );
            }
            other => panic!("expected Apply, got {other:?}"),
        }
    }

    fn apply(text: &str) -> (SmallVec<IpAddr, 4>, SmallVec<u8, 4>, Option<usize>) {
        match analyze_ip_reload_text(text) {
            IpReload::Apply {
                ips,
                weights,
                first_invalid_line,
            } => (ips, weights, first_invalid_line),
            other => panic!("expected Apply, got {other:?}"),
        }
    }

    #[test]
    fn unweighted_file_gets_weight_one_everywhere() {
        let (ips, weights, bad) = apply("10.0.0.1\n10.0.0.2\n");
        assert_eq!(ips.as_slice(), [ip("10.0.0.1"), ip("10.0.0.2")]);
        assert_eq!(weights.as_slice(), [1, 1]);
        assert_eq!(bad, None);
    }

    #[test]
    fn weight_column_is_parsed_and_missing_is_one() {
        let (ips, weights, bad) = apply("192.168.0.15 10\n192.168.0.2\n");
        assert_eq!(ips.as_slice(), [ip("192.168.0.15"), ip("192.168.0.2")]);
        assert_eq!(weights.as_slice(), [10, 1]);
        assert_eq!(bad, None);
    }

    #[test]
    fn weights_are_normalised_so_the_lowest_is_one() {
        // Moblin: priority - lowest + 1.
        let (_, weights, _) = apply("10.0.0.1 10\n10.0.0.2 5\n10.0.0.3 7\n");
        assert_eq!(weights.as_slice(), [6, 1, 3]);
        // Equal weights are no preference at all.
        let (_, weights, _) = apply("10.0.0.1 5\n10.0.0.2 5\n");
        assert_eq!(weights.as_slice(), [1, 1]);
        // A single link is always 1.
        let (_, weights, _) = apply("10.0.0.1 9\n");
        assert_eq!(weights.as_slice(), [1]);
    }

    #[test]
    fn out_of_range_and_bad_weights_keep_the_ip() {
        // 42 clamps to 10; 0, -3 and "x" read as 1 but the IPs still count.
        let (ips, weights, bad) = apply("10.0.0.1 42\n10.0.0.2 0\n10.0.0.3 -3\n10.0.0.4 x\n");
        assert_eq!(ips.len(), 4);
        assert_eq!(weights.as_slice(), [10, 1, 1, 1]);
        assert_eq!(bad, None);
    }

    #[test]
    fn reload_guard_counts_weighted_lines_as_valid() {
        // A file whose only line carries a (bad) weight is still a valid reload.
        let (ips, weights, _) = apply("10.0.0.1 0\n");
        assert_eq!(ips.as_slice(), [ip("10.0.0.1")]);
        assert_eq!(weights.as_slice(), [1]);
        // Garbage IP with a weight is still garbage, and three fields are invalid.
        assert_eq!(
            analyze_ip_reload_text("not-an-ip 10\n10.0.0.1 10 extra\n"),
            IpReload::Refuse(ReloadRefusal::NoValidIps {
                first_invalid_line: 1,
            })
        );
    }

    #[cfg(unix)]
    #[test]
    fn reads_weights_from_a_real_file() {
        let mut f = NamedTempFile::new().unwrap();
        writeln!(f, "127.0.0.1 10").unwrap();
        writeln!(f, "127.0.0.2").unwrap();
        f.flush().unwrap();
        match analyze_ip_reload(f.path().to_str().unwrap()) {
            IpReload::Apply { ips, weights, .. } => {
                assert_eq!(ips.len(), 2);
                assert_eq!(weights.as_slice(), [10, 1]);
            }
            other => panic!("expected Apply, got {other:?}"),
        }
    }
}
