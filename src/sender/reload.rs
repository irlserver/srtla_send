//! SIGHUP IP-list reload guard.
//!
//! Mirrors the C sender's reload guard (`srtla/src/sender_logic.h`,
//! `count_parseable_source_ips` / `analyze_reload_error`): a SIGHUP reload that
//! resolves to zero usable source IPs — a missing/unreadable, empty, or
//! all-garbage file — is REFUSED so the stream keeps running on the existing
//! links instead of tearing every connection down. A file mixing valid and
//! invalid lines still applies; the bad lines are skipped with a warning.

use std::net::IpAddr;
use std::str::FromStr;

use smallvec::SmallVec;

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
    let mut first_invalid_line: Option<usize> = None;
    let mut saw_content = false;

    for (idx, line) in text.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        saw_content = true;
        match IpAddr::from_str(trimmed) {
            Ok(ip) => ips.push(ip),
            Err(_) => {
                if first_invalid_line.is_none() {
                    first_invalid_line = Some(idx + 1);
                }
            }
        }
    }

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
        first_invalid_line,
    }
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
}
