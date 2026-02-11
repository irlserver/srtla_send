use anyhow::{Result, bail};

use crate::topology::Namespace;

/// Gilbert-Elliott (4-state) loss model for `tc netem`.
///
/// Models bursty loss as a Markov chain between Good and Bad states,
/// each with independent loss probabilities.
#[derive(Debug, Clone, Default)]
pub struct GemodelConfig {
    /// Transition probability Good -> Bad (%).
    pub p: f32,
    /// Transition probability Bad -> Good (%).
    pub r: f32,
    /// Loss probability in Good state (%).
    pub one_h: f32,
    /// Loss probability in Bad state (%).
    pub one_k: f32,
}

/// Network impairment applied via `tc netem` (and optionally `tbf`).
///
/// All fields default to `None`/`false`. Set only the parameters you need;
/// omitted parameters are not passed to `tc`. An all-`None` config clears
/// any existing impairment on the interface.
#[derive(Debug, Clone, Default)]
pub struct ImpairmentConfig {
    pub delay_ms: Option<u32>,
    pub jitter_ms: Option<u32>,
    pub loss_percent: Option<f32>,
    pub loss_correlation: Option<f32>,
    /// Overrides `loss_percent` with a Gilbert-Elliott 4-state model.
    pub gemodel: Option<GemodelConfig>,
    pub rate_kbit: Option<u64>,
    pub duplicate_percent: Option<f32>,
    pub reorder_percent: Option<f32>,
    pub corrupt_percent: Option<f32>,
    /// When true, bandwidth is enforced via a TBF root qdisc that drops
    /// excess packets. When false, `rate_kbit` only adds serialization
    /// delay (netem `rate` param) without real enforcement.
    pub tbf_shaping: bool,
}

impl ImpairmentConfig {
    /// True if no impairment parameters are set (config would be a no-op).
    fn is_empty(&self) -> bool {
        self.delay_ms.is_none()
            && self.loss_percent.is_none()
            && self.rate_kbit.is_none()
            && self.gemodel.is_none()
            && self.duplicate_percent.is_none()
            && self.reorder_percent.is_none()
            && self.corrupt_percent.is_none()
    }

    /// True if any netem-specific parameter (delay/loss/dup/reorder/corrupt) is set.
    fn has_netem_params(&self) -> bool {
        self.delay_ms.is_some()
            || self.loss_percent.is_some()
            || self.gemodel.is_some()
            || self.duplicate_percent.is_some()
            || self.reorder_percent.is_some()
            || self.corrupt_percent.is_some()
    }

    /// Build the netem parameter list (delay, loss, dup, reorder, corrupt).
    /// When `include_rate` is true, appends the netem `rate` param too.
    fn netem_args(&self, include_rate: bool) -> Vec<String> {
        let mut args = Vec::new();

        if let Some(delay) = self.delay_ms {
            args.push("delay".into());
            args.push(format!("{delay}ms"));
            if let Some(jitter) = self.jitter_ms
                && jitter > 0
            {
                args.push(format!("{jitter}ms"));
            }
        }

        match (&self.gemodel, self.loss_percent) {
            (Some(ge), _) => {
                args.extend([
                    "loss".into(),
                    "gemodel".into(),
                    format!("{}%", ge.p),
                    format!("{}%", ge.r),
                    format!("{}%", ge.one_h),
                    format!("{}%", ge.one_k),
                ]);
            }
            (None, Some(loss)) => {
                args.push("loss".into());
                args.push(format!("{loss}%"));
                if let Some(corr) = self.loss_correlation {
                    args.push(format!("{corr}%"));
                }
            }
            _ => {}
        }

        if let Some(dup) = self.duplicate_percent {
            args.extend(["duplicate".into(), format!("{dup}%")]);
        }
        if let Some(reorder) = self.reorder_percent {
            args.extend(["reorder".into(), format!("{reorder}%")]);
        }
        if let Some(corrupt) = self.corrupt_percent {
            args.extend(["corrupt".into(), format!("{corrupt}%")]);
        }

        if include_rate
            && let Some(rate) = self.rate_kbit
        {
            args.extend(["rate".into(), format!("{rate}kbit")]);
        }

        args
    }
}

/// Apply impairment to `interface` inside `ns`.
///
/// Always removes the existing root qdisc first (clean slate). With
/// `tbf_shaping`, installs TBF as root for real bandwidth enforcement
/// and chains netem as a child. Without it, netem is the root qdisc.
pub fn apply_impairment(ns: &Namespace, interface: &str, config: ImpairmentConfig) -> Result<()> {
    // Always start clean
    let _ = ns.exec("tc", &["qdisc", "del", "dev", interface, "root"]);

    if config.is_empty() {
        return Ok(());
    }

    if config.tbf_shaping {
        apply_tbf_with_netem(ns, interface, &config)
    } else {
        apply_netem_root(ns, interface, &config)
    }
}

/// TBF as root (bandwidth enforcement) + netem as child (delay/loss).
fn apply_tbf_with_netem(ns: &Namespace, iface: &str, config: &ImpairmentConfig) -> Result<()> {
    let rate = config
        .rate_kbit
        .ok_or_else(|| anyhow::anyhow!("tbf_shaping requires rate_kbit"))?;

    // burst = max(rate_bytes/10, 1540) — at least one MTU
    let rate_bytes_per_sec = rate * 1000 / 8;
    let burst = rate_bytes_per_sec.max(15400) / 10;
    let rate_arg = format!("{rate}kbit");
    let burst_arg = burst.to_string();

    tc_checked(
        ns,
        iface,
        &[
            "qdisc", "add", "dev", iface, "root", "handle", "1:", "tbf", "rate", &rate_arg,
            "burst", &burst_arg, "latency", "1s",
        ],
        "apply TBF qdisc",
    )?;

    if config.has_netem_params() {
        let netem_params = config.netem_args(false);
        let mut args = vec![
            "qdisc", "add", "dev", iface, "parent", "1:1", "handle", "10:", "netem",
        ];
        let netem_strs: Vec<&str> = netem_params.iter().map(|s| s.as_str()).collect();
        args.extend_from_slice(&netem_strs);
        tc_checked(ns, iface, &args, "apply netem child qdisc")?;
    }

    Ok(())
}

/// Netem as root qdisc (no real bandwidth enforcement).
fn apply_netem_root(ns: &Namespace, iface: &str, config: &ImpairmentConfig) -> Result<()> {
    let netem_params = config.netem_args(true);
    let mut args = vec!["qdisc", "add", "dev", iface, "root", "netem"];
    let netem_strs: Vec<&str> = netem_params.iter().map(|s| s.as_str()).collect();
    args.extend_from_slice(&netem_strs);
    tc_checked(ns, iface, &args, "apply netem qdisc")?;
    Ok(())
}

/// Run `tc` inside `ns`, bailing with stderr + the full command on failure.
fn tc_checked(ns: &Namespace, _iface: &str, args: &[&str], ctx: &str) -> Result<()> {
    let output = ns.exec("tc", args)?;
    if !output.status.success() {
        bail!(
            "{ctx}: tc {}\n{}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::{check_privileges, unique_ns_name};

    fn parse_ping_rtt(output: &str) -> Option<f32> {
        output.lines().find_map(|line| {
            let rest = line.split("time=").nth(1)?;
            let num = rest.split_whitespace().next()?;
            num.parse().ok()
        })
    }

    #[test]
    fn test_impairment_delay() {
        if !check_privileges() {
            eprintln!("Skipping: insufficient privileges");
            return;
        }

        let ns1 = Namespace::new(&unique_ns_name("nsi_a")).expect("create ns1");
        let ns2 = Namespace::new(&unique_ns_name("nsi_b")).expect("create ns2");

        ns1.add_veth_link(&ns2, "veth_a", "veth_b", "10.201.1.1/24", "10.201.1.2/24")
            .expect("add veth link");

        let config = ImpairmentConfig {
            delay_ms: Some(100),
            jitter_ms: Some(10),
            rate_kbit: Some(5000),
            ..Default::default()
        };

        if let Err(err) = apply_impairment(&ns1, "veth_a", config) {
            if err.to_string().contains("qdisc kind is unknown") {
                eprintln!("Skipping: netem not available");
                return;
            }
            panic!("apply_impairment: {err}");
        }

        let out = ns1
            .exec("ping", &["-c", "4", "-i", "0.2", "10.201.1.2"])
            .expect("ping");
        assert!(out.status.success(), "ping failed");

        let stdout = String::from_utf8_lossy(&out.stdout);
        let rtt = parse_ping_rtt(&stdout).expect("parse ping RTT");
        assert!(rtt >= 95.0, "RTT {rtt}ms < expected 100ms delay");
    }
}
