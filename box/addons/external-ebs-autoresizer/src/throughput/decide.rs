//! The pure decision: one node's observation in, a recommendation out.

/// EBS gp3 provisioning limits. gp3 is the only volume type this module
/// recommends for, because it is the only one where throughput is provisioned
/// independently: gp2 derives throughput from volume size, and io1/io2 derive
/// it from provisioned IOPS.
pub const VOLUME_TYPE_GP3: &str = "gp3";

pub const GP3_MIN_THROUGHPUT_MIBPS: i32 = 125;
pub const GP3_MAX_THROUGHPUT_MIBPS: i32 = 1000;
const GP3_MIN_IOPS: i32 = 3000;
const GP3_MAX_IOPS: i32 = 16000;

/// Encodes the AWS rule that a gp3 volume may provision at most 0.25 MiB/s of
/// throughput per provisioned IOPS. Throughput therefore cannot be raised
/// past IOPS/4, which is why a throughput recommendation must carry an IOPS
/// recommendation with it.
const GP3_IOPS_PER_MIBPS: i32 = 4;

/// Unit conversion. `DescribeInstanceTypes` reports instance EBS bandwidth
/// in MB/s (decimal megabytes), while gp3 throughput is provisioned in MiB/s
/// (binary mebibytes). Comparing the two directly overstates the instance's
/// headroom by about 4.9%.
const BYTES_PER_MB: f64 = 1_000_000.0;
const BYTES_PER_MIB: f64 = 1_048_576.0;

/// Converts decimal MB/s to binary MiB/s.
#[must_use]
pub fn mbps_to_mibps(mbps: f64) -> f64 {
    mbps * BYTES_PER_MB / BYTES_PER_MIB
}

/// Recommendation actions, published as the recommendation annotation.
pub const ACTION_INCREASE: &str = "increase";
pub const ACTION_DECREASE: &str = "decrease";
pub const ACTION_NONE: &str = "none";
pub const ACTION_UNKNOWN: &str = "unknown";

/// Reasons explaining an action, published as the
/// throughput-recommendation-reason annotation. Every `ACTION_UNKNOWN`
/// carries one of the non-fitted reasons.
pub const REASON_FITS: &str = "observed_peak_within_provisioned";
pub const REASON_BELOW_PROVISIONED: &str = "observed_peak_far_below_provisioned";
pub const REASON_ABOVE_PROVISIONED: &str = "observed_peak_above_provisioned";
pub const REASON_INSTANCE_BANDWIDTH_CAP: &str = "clamped_to_instance_bandwidth";
pub const REASON_VOLUME_MAX_CAP: &str = "clamped_to_gp3_maximum";
pub const REASON_INSUFFICIENT_DATA: &str = "insufficient_samples";
pub const REASON_NODE_TOO_YOUNG: &str = "node_younger_than_window";
pub const REASON_UNSUPPORTED_VOLUME_TYPE: &str = "unsupported_volume_type";
pub const REASON_MULTIPLE_VOLUMES: &str = "multiple_attached_volumes";
pub const REASON_NO_VOLUME: &str = "no_attached_volume";
pub const REASON_NOT_EC2_NODE: &str = "not_an_ec2_node";
pub const REASON_NO_METRICS: &str = "no_metrics_for_node";

/// The tunables of the decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Settings {
    /// Added on top of the observed peak so the recommendation leaves room
    /// above measured demand.
    pub headroom_percent: i32,
    /// Quantizes the recommendation and provides the hysteresis that keeps it
    /// from flapping: a decrease is only recommended when the target is at
    /// least one full step below the current value.
    pub step_mibps: i32,
    /// A floor below which no recommendation is made, never less than the gp3
    /// minimum of 125.
    pub min_throughput_mibps: i32,
    /// A ceiling, never more than the gp3 maximum of 1000.
    pub max_throughput_mibps: i32,
    /// How many data points the observation window must contain before a
    /// recommendation is trusted.
    pub min_samples: usize,
}

/// Everything the decision needs about one node's volume.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Input {
    pub volume_type: String,
    pub current_throughput_mibps: i32,
    pub current_iops: i32,
    /// The observed peak throughput of the node in MiB/s (the configured
    /// quantile over the observation window, not the mean).
    pub peak_mibps: f64,
    /// How many data points backed `peak_mibps`.
    pub samples: usize,
    /// The instance type's EBS bandwidth ceiling in MiB/s. Zero means
    /// unknown, in which case no instance clamp is applied.
    pub instance_max_mibps: f64,
    /// The bandwidth the instance sustains indefinitely, in MiB/s. Zero means
    /// unknown. It does not clamp the recommendation, but it is reported.
    pub instance_baseline_mibps: f64,
}

/// The outcome of one node's evaluation.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Decision {
    pub action: String,
    pub reason: String,
    /// Only meaningful when `action` is increase or decrease.
    /// `recommended_iops` equals the current IOPS unless the gp3
    /// throughput-to-IOPS ratio forces an IOPS bump as well.
    pub recommended_throughput_mibps: i32,
    pub recommended_iops: i32,
    /// The observed demand asked for more throughput than the recommendation
    /// grants, because a ceiling intervened. `reason` names the ceiling.
    pub capped: bool,
}

impl Decision {
    fn unknown(reason: &str) -> Self {
        Self {
            action: ACTION_UNKNOWN.into(),
            reason: reason.into(),
            ..Self::default()
        }
    }
}

/// Turns one node's observation into a recommendation. It is pure: no clock,
/// no I/O, no AWS.
#[must_use]
pub fn decide(input: &Input, s: &Settings) -> Decision {
    if input.volume_type != VOLUME_TYPE_GP3 {
        return Decision::unknown(REASON_UNSUPPORTED_VOLUME_TYPE);
    }
    if input.samples < s.min_samples {
        return Decision::unknown(REASON_INSUFFICIENT_DATA);
    }
    // A NaN peak reaches here when the query returned a value for the node
    // but the underlying series had no data in the window. Treating it as
    // zero would recommend the floor; it is missing data, not idleness.
    if !input.peak_mibps.is_finite() || input.peak_mibps < 0.0 {
        return Decision::unknown(REASON_INSUFFICIENT_DATA);
    }

    let desired = input.peak_mibps * (1.0 + f64::from(s.headroom_percent) / 100.0);
    let mut target = ceil_to_step(desired, s.step_mibps);

    let (floor, ceiling, cap_reason) = bounds(s, input.instance_max_mibps);
    let capped = target > ceiling;
    target = target.max(floor).min(ceiling);

    let mut d = Decision {
        recommended_throughput_mibps: target,
        recommended_iops: required_iops(target, input.current_iops),
        capped,
        ..Decision::default()
    };
    if target > input.current_throughput_mibps {
        d.action = ACTION_INCREASE.into();
        d.reason = if capped {
            cap_reason
        } else {
            REASON_ABOVE_PROVISIONED
        }
        .into();
    } else if target <= input.current_throughput_mibps - s.step_mibps {
        d.action = ACTION_DECREASE.into();
        d.reason = REASON_BELOW_PROVISIONED.into();
    } else {
        d.action = ACTION_NONE.into();
        // Nothing to change, so the recommendation is the current
        // provisioning. Reporting the computed target here would read as a
        // pending change.
        d.reason = if capped { cap_reason } else { REASON_FITS }.into();
        d.recommended_throughput_mibps = input.current_throughput_mibps;
        d.recommended_iops = input.current_iops;
    }
    d
}

/// Resolves the effective floor and ceiling of a recommendation: the
/// configured range, tightened to the gp3 limits and to what the instance
/// type can actually drive. The reason names whichever ceiling is binding.
fn bounds(s: &Settings, instance_max_mibps: f64) -> (i32, i32, &'static str) {
    let mut floor = s.min_throughput_mibps.max(GP3_MIN_THROUGHPUT_MIBPS);
    let mut ceiling = GP3_MAX_THROUGHPUT_MIBPS;
    let mut cap_reason = REASON_VOLUME_MAX_CAP;
    if s.max_throughput_mibps > 0 && s.max_throughput_mibps < ceiling {
        ceiling = s.max_throughput_mibps;
    }
    // The instance clamp is checked last and reported only when it is
    // strictly tighter than the volume-side ceiling.
    if instance_max_mibps > 0.0 {
        #[allow(clippy::cast_possible_truncation)]
        let instance_ceiling = instance_max_mibps.floor() as i32;
        if instance_ceiling < ceiling {
            ceiling = instance_ceiling;
            cap_reason = REASON_INSTANCE_BANDWIDTH_CAP;
        }
    }
    // A misconfigured range (floor above ceiling) collapses to the ceiling.
    if floor > ceiling {
        floor = ceiling;
    }
    (floor, ceiling, cap_reason)
}

/// Returns the IOPS the recommended throughput needs. IOPS is never
/// recommended downward here: it is a separate dimension with its own demand
/// signal.
fn required_iops(throughput_mibps: i32, current_iops: i32) -> i32 {
    let needed = throughput_mibps * GP3_IOPS_PER_MIBPS;
    current_iops.max(needed).clamp(GP3_MIN_IOPS, GP3_MAX_IOPS)
}

/// Rounds `v` up to the next multiple of `step`. A non-positive step means no
/// quantization.
#[allow(clippy::cast_possible_truncation)]
fn ceil_to_step(v: f64, step: i32) -> i32 {
    if step <= 0 {
        return v.ceil() as i32;
    }
    let steps = (v / f64::from(step)).ceil() as i32;
    steps * step
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings() -> Settings {
        Settings {
            headroom_percent: 30,
            step_mibps: 125,
            min_throughput_mibps: 125,
            max_throughput_mibps: 1000,
            min_samples: 10,
        }
    }

    fn input(peak: f64, current: i32, iops: i32) -> Input {
        Input {
            volume_type: VOLUME_TYPE_GP3.into(),
            current_throughput_mibps: current,
            current_iops: iops,
            peak_mibps: peak,
            samples: 100,
            instance_max_mibps: 0.0,
            instance_baseline_mibps: 0.0,
        }
    }

    #[test]
    fn decisions() {
        let s = settings();
        let d = decide(&input(200.0, 125, 3000), &s);
        assert_eq!(
            (d.action.as_str(), d.reason.as_str()),
            (ACTION_INCREASE, REASON_ABOVE_PROVISIONED)
        );
        assert_eq!(
            d.recommended_throughput_mibps, 375,
            "260 desired rounds up to a step"
        );
        assert_eq!(d.recommended_iops, 3000, "375*4=1500 fits within 3000");
        assert!(!d.capped);

        let d = decide(&input(50.0, 125, 3000), &s);
        assert_eq!(
            (d.action.as_str(), d.reason.as_str()),
            (ACTION_NONE, REASON_FITS)
        );
        assert_eq!(d.recommended_throughput_mibps, 125);
        assert_eq!(d.recommended_iops, 3000);

        let d = decide(&input(50.0, 500, 4000), &s);
        assert_eq!(
            (d.action.as_str(), d.reason.as_str()),
            (ACTION_DECREASE, REASON_BELOW_PROVISIONED)
        );
        assert_eq!(d.recommended_throughput_mibps, 125);
        assert_eq!(d.recommended_iops, 4000, "IOPS never lowered");

        // Less than one full step of slack: hysteresis keeps it at none.
        let d = decide(&input(50.0, 200, 3000), &s);
        assert_eq!(d.action, ACTION_NONE);
        // Exactly one step of slack is enough for a decrease.
        assert_eq!(decide(&input(50.0, 250, 3000), &s).action, ACTION_DECREASE);

        let d = decide(&input(900.0, 500, 3000), &s);
        assert_eq!(
            (d.action.as_str(), d.reason.as_str()),
            (ACTION_INCREASE, REASON_VOLUME_MAX_CAP)
        );
        assert_eq!(d.recommended_throughput_mibps, 1000);
        assert_eq!(d.recommended_iops, 4000, "1000 MiB/s needs 4000 IOPS");
        assert!(d.capped);

        let d = decide(&input(900.0, 1000, 4000), &s);
        assert_eq!(
            (d.action.as_str(), d.reason.as_str()),
            (ACTION_NONE, REASON_VOLUME_MAX_CAP)
        );
        assert!(d.capped, "already at the ceiling while demand exceeds it");

        let mut in_ = input(400.0, 125, 3000);
        in_.instance_max_mibps = 300.7;
        let d = decide(&in_, &s);
        assert_eq!(
            (d.action.as_str(), d.reason.as_str()),
            (ACTION_INCREASE, REASON_INSTANCE_BANDWIDTH_CAP)
        );
        assert_eq!(d.recommended_throughput_mibps, 300);

        let d = decide(&input(4000.0, 125, 16000), &s);
        assert_eq!(d.recommended_iops, 16000, "IOPS ceiling");
    }

    #[test]
    fn unknowns() {
        let s = settings();
        let mut in_ = input(200.0, 125, 3000);
        in_.volume_type = "gp2".into();
        assert_eq!(decide(&in_, &s).reason, REASON_UNSUPPORTED_VOLUME_TYPE);
        let mut in_ = input(200.0, 125, 3000);
        in_.samples = 9;
        let d = decide(&in_, &s);
        assert_eq!(
            (d.action.as_str(), d.reason.as_str()),
            (ACTION_UNKNOWN, REASON_INSUFFICIENT_DATA)
        );
        for peak in [f64::NAN, f64::INFINITY, -1.0] {
            assert_eq!(
                decide(&input(peak, 125, 3000), &s).reason,
                REASON_INSUFFICIENT_DATA
            );
        }
    }

    #[test]
    fn recommendation_is_always_provisionable() {
        let s = settings();
        for peak in [0.0, 1.0, 100.0, 333.3, 999.0, 5000.0] {
            for max in [0.0, 200.0, 593.75, 2000.0] {
                let mut in_ = input(peak, 125, 3000);
                in_.instance_max_mibps = max;
                let d = decide(&in_, &s);
                let t = d.recommended_throughput_mibps;
                assert!(
                    (GP3_MIN_THROUGHPUT_MIBPS..=GP3_MAX_THROUGHPUT_MIBPS).contains(&t),
                    "{peak} {max} {t}"
                );
                assert!(
                    d.recommended_iops >= t * GP3_IOPS_PER_MIBPS
                        || d.recommended_iops == GP3_MAX_IOPS
                );
                assert!((GP3_MIN_IOPS..=GP3_MAX_IOPS).contains(&d.recommended_iops));
            }
        }
    }

    #[test]
    fn helpers() {
        assert_eq!(ceil_to_step(0.0, 125), 0);
        assert_eq!(ceil_to_step(1.0, 125), 125);
        assert_eq!(ceil_to_step(125.0, 125), 125);
        assert_eq!(ceil_to_step(126.0, 125), 250);
        assert_eq!(ceil_to_step(10.2, 0), 11);
        assert!((mbps_to_mibps(1000.0) - 953.674).abs() < 0.001);
        let mut s = settings();
        s.min_throughput_mibps = 800;
        s.max_throughput_mibps = 500;
        assert_eq!(
            bounds(&s, 0.0),
            (500, 500, REASON_VOLUME_MAX_CAP),
            "misconfigured range collapses"
        );
        assert_eq!(
            bounds(&settings(), 5000.0),
            (125, 1000, REASON_VOLUME_MAX_CAP),
            "instance cap above gp3 max is not reported"
        );
        assert_eq!(
            bounds(&settings(), 400.9),
            (125, 400, REASON_INSTANCE_BANDWIDTH_CAP)
        );
        assert_eq!(required_iops(125, 0), 3000);
        assert_eq!(required_iops(1000, 3000), 4000);
        assert_eq!(required_iops(1000, 10000), 10000);
    }
}
