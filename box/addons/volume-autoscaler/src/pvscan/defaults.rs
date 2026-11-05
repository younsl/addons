//! Fixed policy. The scanner has no configuration surface: every value below
//! is a property of how Kubernetes behaves or a judgement call that does not
//! vary per cluster. Exposing them would only add ways to configure the scan
//! into reporting nothing, on a loop whose entire output is advisory and
//! whose cost is four list calls an hour.

use std::time::Duration;

/// How often the scan runs. What it reports changes only when workloads are
/// deleted, and every finding is held back a day by `MIN_UNUSED_AGE` anyway,
/// so a shorter interval would re-list the whole cluster to reach the same
/// answer. It is also the refresh cadence of the standing
/// `UnusedVolumeDetected` Event, which the API server expires after
/// `--event-ttl` (one hour by default).
pub const INTERVAL: Duration = Duration::from_hours(1);

/// How long an object must have been continuously unused before it is
/// reported. Most of what a single pass sees as unused is a workload between
/// two Pods, so a shorter threshold reports mostly churn. The clock is
/// persisted in the object's own annotation, so it survives a restart.
pub const MIN_UNUSED_AGE: Duration = Duration::from_hours(24);
