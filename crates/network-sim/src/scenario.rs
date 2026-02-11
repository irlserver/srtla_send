use std::time::Duration;

use rand::rngs::StdRng;
use rand::{RngExt as _, SeedableRng};

use crate::impairment::ImpairmentConfig;

/// Top-level scenario configuration.
#[derive(Debug, Clone)]
pub struct ScenarioConfig {
    /// RNG seed for reproducibility.
    pub seed: u64,
    /// Total scenario duration.
    pub duration: Duration,
    /// Time between frames.
    pub step: Duration,
    /// Per-link random-walk bounds.
    pub links: Vec<LinkScenarioConfig>,
}

/// Per-link bounds and maximum step sizes for the random walk.
#[derive(Debug, Clone)]
pub struct LinkScenarioConfig {
    pub min_rate_kbit: u64,
    pub max_rate_kbit: u64,
    pub rate_step_kbit: u64,
    pub base_delay_ms: u32,
    pub delay_jitter_ms: u32,
    pub delay_step_ms: u32,
    pub max_loss_percent: f32,
    pub loss_step_percent: f32,
}

/// One time-step of impairment values for every link.
#[derive(Debug, Clone)]
pub struct ScenarioFrame {
    pub t: Duration,
    pub configs: Vec<ImpairmentConfig>,
}

/// Deterministic random-walk impairment generator.
///
/// Given a seed, produces a reproducible sequence of [`ScenarioFrame`]s where
/// each link's rate, delay, and loss evolve via clamped random-walk steps.
#[derive(Debug)]
pub struct Scenario {
    cfg: ScenarioConfig,
    rng: StdRng,
    states: Vec<LinkState>,
}

#[derive(Debug, Clone)]
struct LinkState {
    rate_kbit: f64,
    delay_ms: f64,
    loss_percent: f64,
}

impl Scenario {
    pub fn new(cfg: ScenarioConfig) -> Self {
        let mut rng = StdRng::seed_from_u64(cfg.seed);

        let states = cfg
            .links
            .iter()
            .map(|link| {
                let range = link.max_rate_kbit.saturating_sub(link.min_rate_kbit) as f64;
                LinkState {
                    rate_kbit: link.min_rate_kbit as f64 + rng.random::<f64>() * range,
                    delay_ms: link.base_delay_ms as f64,
                    loss_percent: rng.random::<f64>() * link.max_loss_percent as f64 * 0.2,
                }
            })
            .collect();

        Self { cfg, rng, states }
    }

    /// Generate all frames for the configured duration.
    pub fn frames(&mut self) -> Vec<ScenarioFrame> {
        let total_steps =
            (self.cfg.duration.as_secs_f64() / self.cfg.step.as_secs_f64()).ceil() as u64;

        (0..=total_steps)
            .map(|step_idx| {
                let t = self.cfg.step.mul_f64(step_idx as f64);
                let configs = self
                    .cfg
                    .links
                    .iter()
                    .zip(self.states.iter_mut())
                    .map(|(link_cfg, state)| {
                        state.rate_kbit = (state.rate_kbit
                            + rand_signed(&mut self.rng, link_cfg.rate_step_kbit as f64))
                        .clamp(link_cfg.min_rate_kbit as f64, link_cfg.max_rate_kbit as f64);

                        let max_delay =
                            (link_cfg.base_delay_ms + link_cfg.delay_jitter_ms) as f64;
                        state.delay_ms = (state.delay_ms
                            + rand_signed(&mut self.rng, link_cfg.delay_step_ms as f64))
                        .clamp(1.0, max_delay);

                        state.loss_percent = (state.loss_percent
                            + rand_signed(&mut self.rng, link_cfg.loss_step_percent as f64))
                        .clamp(0.0, link_cfg.max_loss_percent as f64);

                        ImpairmentConfig {
                            rate_kbit: Some(state.rate_kbit.max(1.0) as u64),
                            delay_ms: Some(state.delay_ms.max(1.0) as u32),
                            jitter_ms: (link_cfg.delay_jitter_ms > 0)
                                .then_some(link_cfg.delay_jitter_ms),
                            loss_percent: Some(state.loss_percent as f32),
                            ..Default::default()
                        }
                    })
                    .collect();

                ScenarioFrame { t, configs }
            })
            .collect()
    }
}

/// Random value in `[-max_step, +max_step]`.
fn rand_signed(rng: &mut StdRng, max_step: f64) -> f64 {
    if max_step <= 0.0 {
        return 0.0;
    }
    let mag = rng.random::<f64>() * max_step;
    if rng.random::<bool>() { mag } else { -mag }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn two_link_config() -> ScenarioConfig {
        ScenarioConfig {
            seed: 42,
            duration: Duration::from_secs(5),
            step: Duration::from_secs(1),
            links: vec![
                LinkScenarioConfig {
                    min_rate_kbit: 500,
                    max_rate_kbit: 1500,
                    rate_step_kbit: 150,
                    base_delay_ms: 30,
                    delay_jitter_ms: 20,
                    delay_step_ms: 5,
                    max_loss_percent: 10.0,
                    loss_step_percent: 2.0,
                },
                LinkScenarioConfig {
                    min_rate_kbit: 800,
                    max_rate_kbit: 2000,
                    rate_step_kbit: 200,
                    base_delay_ms: 20,
                    delay_jitter_ms: 10,
                    delay_step_ms: 4,
                    max_loss_percent: 5.0,
                    loss_step_percent: 1.0,
                },
            ],
        }
    }

    #[test]
    fn deterministic_for_same_seed() {
        let f1 = Scenario::new(two_link_config()).frames();
        let f2 = Scenario::new(two_link_config()).frames();

        assert_eq!(f1.len(), f2.len());
        for (a, b) in f1.iter().zip(&f2) {
            assert_eq!(a.t, b.t);
            for (ca, cb) in a.configs.iter().zip(&b.configs) {
                assert_eq!(ca.rate_kbit, cb.rate_kbit);
                assert_eq!(ca.delay_ms, cb.delay_ms);
                assert_eq!(ca.loss_percent, cb.loss_percent);
            }
        }
    }

    #[test]
    fn values_stay_within_bounds() {
        let cfg = two_link_config();
        let frames = Scenario::new(cfg.clone()).frames();

        for frame in &frames {
            for (config, link_cfg) in frame.configs.iter().zip(&cfg.links) {
                let rate = config.rate_kbit.unwrap();
                assert!(rate >= link_cfg.min_rate_kbit, "rate {rate} < min");
                assert!(rate <= link_cfg.max_rate_kbit, "rate {rate} > max");

                let loss = config.loss_percent.unwrap();
                assert!(loss >= 0.0, "negative loss");
                assert!(loss <= link_cfg.max_loss_percent, "loss {loss} > max");
            }
        }
    }
}
