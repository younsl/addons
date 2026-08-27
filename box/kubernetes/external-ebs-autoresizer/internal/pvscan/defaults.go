package pvscan

import "time"

// Fixed policy. The scanner has no configuration surface: every value below is a
// property of how Kubernetes behaves or a judgement call that does not vary per
// cluster, the same reasoning that keeps the throughput recommender's decision
// tunables out of the config file. Exposing them would only add ways to
// configure the scan into reporting nothing, on a loop whose entire output is
// advisory and whose cost is four list calls an hour.
const (
	// Interval is how often the scan runs. What it reports changes only when
	// workloads are deleted, and every finding is held back a day by
	// MinUnusedAge anyway, so a shorter interval would re-list the whole cluster
	// to reach the same answer.
	//
	// It is also the refresh cadence of the standing UnusedVolumeDetected Event,
	// which the API server expires after --event-ttl (one hour by default). The
	// two therefore race, and an Event can lapse briefly between passes. The
	// annotation and the metrics do not expire and are the durable record.
	Interval = time.Hour

	// MinUnusedAge is how long an object must have been continuously unused
	// before it is reported. Most of what a single pass sees as unused is a
	// workload between two Pods, so a shorter threshold reports mostly churn. The
	// clock is persisted in the object's own annotation, so it survives a
	// restart of the controller.
	MinUnusedAge = 24 * time.Hour
)
