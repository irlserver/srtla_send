use std::net::SocketAddr;

use tracing::{info, warn};

/// How often a continuing outage repeats its drop count.
pub(super) const NO_LINK_SUMMARY_INTERVAL_MS: u64 = 5_000;

#[derive(Debug, PartialEq, Eq)]
pub(super) enum NoLinkEvent {
    /// The first packet of an outage found no link.
    Started,
    /// The outage continues. `dropped` counts since the last report.
    Continuing {
        dropped: u64,
        total: u64,
        for_ms: u64,
    },
    /// A packet was forwarded again after an outage.
    Ended { total: u64, for_ms: u64 },
}

#[derive(Debug)]
struct Outage {
    started_ms: u64,
    last_report_ms: u64,
    dropped_since_report: u64,
    dropped_total: u64,
}

#[derive(Debug, Default)]
pub(super) struct NoLinkLog {
    outage: Option<Outage>,
}

impl NoLinkLog {
    pub(super) fn on_dropped(&mut self, now: u64) -> Option<NoLinkEvent> {
        let Some(outage) = self.outage.as_mut() else {
            // The `Started` line reports the first packet, so it counts in the
            // total but not in the next summary.
            self.outage = Some(Outage {
                started_ms: now,
                last_report_ms: now,
                dropped_since_report: 0,
                dropped_total: 1,
            });
            return Some(NoLinkEvent::Started);
        };
        outage.dropped_since_report += 1;
        outage.dropped_total += 1;
        if now.saturating_sub(outage.last_report_ms) < NO_LINK_SUMMARY_INTERVAL_MS {
            return None;
        }
        let event = NoLinkEvent::Continuing {
            dropped: outage.dropped_since_report,
            total: outage.dropped_total,
            for_ms: now.saturating_sub(outage.started_ms),
        };
        outage.last_report_ms = now;
        outage.dropped_since_report = 0;
        Some(event)
    }

    pub(super) fn on_forwarded(&mut self, now: u64) -> Option<NoLinkEvent> {
        self.outage.take().map(|o| NoLinkEvent::Ended {
            total: o.dropped_total,
            for_ms: now.saturating_sub(o.started_ms),
        })
    }
}

pub(super) fn log_event(event: NoLinkEvent, src: SocketAddr) {
    match event {
        NoLinkEvent::Started => {
            warn!("no uplink can carry traffic; dropping SRT packets from {src}")
        }
        NoLinkEvent::Continuing {
            dropped,
            total,
            for_ms,
        } => warn!(
            "still no uplink after {for_ms} ms; dropped {dropped} packets since the last report \
             ({total} total)"
        ),
        NoLinkEvent::Ended { total, for_ms } => {
            info!("uplink available again after {for_ms} ms; {total} SRT packets were dropped")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_drop_starts_an_outage() {
        let mut log = NoLinkLog::default();
        assert_eq!(log.on_dropped(1_000), Some(NoLinkEvent::Started));
        assert_eq!(log.on_dropped(1_001), None);
    }

    #[test]
    fn continuing_outage_reports_every_interval() {
        let mut log = NoLinkLog::default();
        assert_eq!(log.on_dropped(1_000), Some(NoLinkEvent::Started));
        for t in 1_001..=1_100 {
            assert_eq!(log.on_dropped(t), None);
        }
        assert_eq!(
            log.on_dropped(6_000),
            Some(NoLinkEvent::Continuing {
                dropped: 101,
                total: 102,
                for_ms: 5_000,
            })
        );
        assert_eq!(log.on_dropped(6_001), None);
    }

    #[test]
    fn forward_after_outage_ends_it_once() {
        let mut log = NoLinkLog::default();
        log.on_dropped(1_000);
        log.on_dropped(1_500);
        assert_eq!(
            log.on_forwarded(3_000),
            Some(NoLinkEvent::Ended {
                total: 2,
                for_ms: 2_000,
            })
        );
        assert_eq!(log.on_forwarded(3_001), None);
    }

    #[test]
    fn forward_without_outage_is_silent() {
        assert_eq!(NoLinkLog::default().on_forwarded(1_000), None);
    }

    #[test]
    fn next_outage_starts_fresh() {
        let mut log = NoLinkLog::default();
        log.on_dropped(1_000);
        log.on_dropped(1_500);
        log.on_forwarded(2_000);
        assert_eq!(log.on_dropped(9_000), Some(NoLinkEvent::Started));
        assert_eq!(
            log.on_forwarded(9_500),
            Some(NoLinkEvent::Ended {
                total: 1,
                for_ms: 500,
            })
        );
    }
}
