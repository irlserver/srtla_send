//! Time-windowed running minimum in constant space.
//!
//! Kathleen Nichols' algorithm, as used by BBR for its min-RTT and max-bandwidth
//! filters (Linux `lib/win_minmax.c`). It keeps the best, second-best and
//! third-best samples from successive sub-windows rather than every sample, so
//! memory and per-sample cost stay fixed no matter how fast samples arrive.
//!
//! The window must be measured in time, not samples. RTT samples arrive at the
//! link's ACK rate, which runs from one per second on an idle link to hundreds
//! per second on a loaded one, so a fixed sample count covers anything from
//! minutes to a fraction of a second. On a loaded link with a standing queue, a
//! sub-second window never sees the propagation floor and reports the queue
//! delay as the "minimum".
//!
//! The estimate is approximate: when the best sample expires, the next best
//! may come from a later sub-window and sit slightly above the true windowed
//! minimum. It never reports a sample older than the window.

#[derive(Debug, Clone, Copy)]
struct Sample {
    t_ms: u64,
    v: f64,
}

#[derive(Debug, Clone)]
pub struct WindowedMin {
    window_ms: u64,
    s: Option<[Sample; 3]>,
}

impl WindowedMin {
    pub const fn new(window_ms: u64) -> Self {
        Self { window_ms, s: None }
    }

    pub fn reset(&mut self) {
        self.s = None;
    }

    /// Current windowed minimum, or `None` before the first sample.
    pub fn get(&self) -> Option<f64> {
        self.s.map(|s| s[0].v)
    }

    /// Fold in a sample taken at `now_ms` and return the updated minimum.
    pub fn update(&mut self, now_ms: u64, v: f64) -> f64 {
        let val = Sample { t_ms: now_ms, v };
        let win = self.window_ms;
        let s = match &mut self.s {
            Some(s) if v > s[0].v && now_ms.saturating_sub(s[2].t_ms) <= win => s,
            _ => {
                self.s = Some([val; 3]);
                return v;
            }
        };

        if v <= s[1].v {
            s[1] = val;
            s[2] = val;
        } else if v <= s[2].v {
            s[2] = val;
        }

        let dt = now_ms.saturating_sub(s[0].t_ms);
        if dt > win {
            // The best sample expired: promote the runners-up. The second
            // best can be expired too when samples are sparse, so check again.
            s[0] = s[1];
            s[1] = s[2];
            s[2] = val;
            if now_ms.saturating_sub(s[0].t_ms) > win {
                s[0] = s[1];
                s[1] = s[2];
                s[2] = val;
            }
        } else if s[1].t_ms == s[0].t_ms && dt > win / 4 {
            // A quarter window passed with no new second best: take one from
            // this sub-window so a later expiry has a recent fallback.
            s[1] = val;
            s[2] = val;
        } else if s[2].t_ms == s[1].t_ms && dt > win / 2 {
            s[2] = val;
        }
        s[0].v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T0: u64 = 1_000_000;

    #[test]
    fn tracks_a_lower_sample_immediately() {
        let mut m = WindowedMin::new(1000);
        assert_eq!(m.get(), None);
        assert_eq!(m.update(T0, 50.0), 50.0);
        assert_eq!(m.update(T0 + 10, 80.0), 50.0);
        assert_eq!(m.update(T0 + 20, 30.0), 30.0);
    }

    #[test]
    fn sample_rate_does_not_shorten_the_window() {
        // A loaded link answers hundreds of ACKs a second. A floor sample must
        // still be held for the whole window, however many samples land on top.
        let mut m = WindowedMin::new(30_000);
        m.update(T0, 20.0);
        for i in 1..=25_000u64 {
            m.update(T0 + i, 600.0);
        }
        assert_eq!(m.get(), Some(20.0));
    }

    #[test]
    fn floor_expires_after_the_window() {
        let mut m = WindowedMin::new(3000);
        m.update(T0, 20.0);
        let mut t = T0;
        while t < T0 + 3000 {
            t += 100;
            m.update(t, 100.0);
        }
        assert_eq!(m.update(t + 100, 100.0), 100.0);
    }

    #[test]
    fn falls_back_to_a_recent_runner_up_not_the_newest_sample() {
        // After the best expires, the minimum is the best of what is still in
        // the window, not whatever sample happened to arrive last.
        let mut m = WindowedMin::new(1000);
        m.update(T0, 10.0);
        m.update(T0 + 300, 40.0);
        m.update(T0 + 600, 70.0);
        assert_eq!(m.update(T0 + 1100, 90.0), 40.0);
    }

    #[test]
    fn sparse_samples_never_report_an_expired_value() {
        let mut m = WindowedMin::new(1000);
        m.update(T0, 10.0);
        m.update(T0 + 100, 20.0);
        assert_eq!(m.update(T0 + 5000, 90.0), 90.0);
    }

    #[test]
    fn reset_forgets_everything() {
        let mut m = WindowedMin::new(1000);
        m.update(T0, 10.0);
        m.reset();
        assert_eq!(m.get(), None);
        assert_eq!(m.update(T0 + 1, 50.0), 50.0);
    }
}
