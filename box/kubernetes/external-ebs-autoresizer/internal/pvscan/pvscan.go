// Package pvscan identifies PersistentVolumeClaims and PersistentVolumes that no
// workload is using, so the EBS volumes behind them stop billing unnoticed.
//
// It only ever identifies. Nothing here deletes a claim or a volume, and the
// package has no access to one: the Kubernetes surface it depends on is list and
// annotate, and it never touches EC2 at all. That separation is deliberate.
// "Unused" is an observation about the current state of the cluster, not a
// statement that the data is disposable: a claim held for a quarterly job, a
// StatefulSet parked at zero replicas, and a volume kept for a restore all look
// identical from here. Deleting on that inference would be the one mistake in
// this addon that no later pass can undo, so the decision stays with an operator
// reading the annotation.
//
// The report is deliberately conservative about what counts as in use. A claim
// mounted by a Pod in a terminal phase is unused (a completed Job's Pod object
// outlives its run and mounts nothing), while a volumeClaimTemplate claim inside
// its StatefulSet's replica range is in use even with no Pod at all, because
// every rolling update passes through that gap.
package pvscan

import (
	"context"
	"fmt"
	"log/slog"
	"time"

	corev1 "k8s.io/api/core/v1"
)

// KubeAPI is the subset of Kubernetes operations this package depends on.
// Client implements it. Every operation is a list or an annotation patch: there
// is no delete, by construction.
type KubeAPI interface {
	Inventory(ctx context.Context) (Inventory, error)
	AnnotatePVC(ctx context.Context, namespace, name string, set map[string]string, remove []string) error
	AnnotatePV(ctx context.Context, name string, set map[string]string, remove []string) error
}

// EventEmitter publishes Kubernetes Events against the claims and volumes the
// scanner reports on. events.ObjectEmitter implements it. A nil EventEmitter
// disables Events (running outside a cluster, or during tests).
type EventEmitter interface {
	ClaimEventf(namespace, name, uid, eventType, reason, messageFmt string, args ...any)
	VolumeEventf(name, uid, eventType, reason, messageFmt string, args ...any)
}

// Kubernetes Event reasons published against a claim or a volume.
//
// Both are Normal, not Warning. Nothing about the object is broken: an unused
// claim is a cost observation, and a cluster with a hundred of them would drown
// the Warnings that do mean something is failing.
const (
	// reasonUnusedDetected is republished for a reported object on every pass. The
	// recorder aggregates a repeat into the existing Event and bumps its count, so
	// the cost is one Event object per finding rather than one per pass, and the
	// republishing is what keeps the Event from lapsing under the API server's
	// event TTL while the object is still unused.
	reasonUnusedDetected = "UnusedVolumeDetected"
	// reasonUnusedCleared marks the transition back into use. Unlike the detected
	// Event it fires once, on the pass that erases the mark, because that is a
	// thing that happened rather than a state that persists.
	reasonUnusedCleared = "UnusedVolumeCleared"
)

// Recorder receives metrics observations. observability.Metrics implements it.
type Recorder interface {
	ResetUnusedVolumes()
	ObserveUnusedPVC(namespace, name, volumeName, volumeID, storageClass, reason string, ageSeconds, capacityBytes float64)
	ObserveUnusedPV(name, volumeID, storageClass, reason, reclaimPolicy, claimNamespace, claimName string, ageSeconds, capacityBytes float64)
	ObserveUnusedSummary(kind, reason string, count int, capacityBytes int64)
	ObserveError(stage string)
}

// Scanner evaluates every claim and volume in the cluster once per pass.
type Scanner struct {
	// dryRun suppresses the annotation patches and the Events. The scan itself
	// still runs and still reports: reading the cluster is not a mutation. It is
	// the only thing about the scanner an operator can influence, and it is
	// inherited from the global switch rather than declared per loop.
	dryRun bool
	kube   KubeAPI
	rec    Recorder
	events EventEmitter
	logger *slog.Logger
	// minUnusedAge is MinUnusedAge, held as a field so tests can exercise the
	// threshold without waiting a day. Nothing outside this package sets it.
	minUnusedAge time.Duration
	// now is injectable so tests control the unused clock.
	now func() time.Time
}

// New constructs a Scanner. events may be nil to disable Kubernetes Events.
func New(dryRun bool, kube KubeAPI, rec Recorder, events EventEmitter, logger *slog.Logger) *Scanner {
	return &Scanner{
		dryRun:       dryRun,
		kube:         kube,
		rec:          rec,
		events:       events,
		logger:       logger,
		minUnusedAge: MinUnusedAge,
		now:          time.Now,
	}
}

// Reconcile classifies every claim and volume and publishes the result,
// returning the number of objects considered. The whole cluster is read in four
// list calls rather than per object, so pass cost stays flat as it grows.
// Per-object annotation failures are logged and counted but never abort the pass.
func (s *Scanner) Reconcile(ctx context.Context) (int, error) {
	inv, err := s.kube.Inventory(ctx)
	if err != nil {
		s.rec.ObserveError("pv_inventory")
		return 0, fmt.Errorf("read volume inventory: %w", err)
	}

	now := s.now()
	findings := classify(inv, s.minUnusedAge, now)

	// The reset is what keeps a deleted claim from staying exported forever and
	// reading as a live one, the same problem the recommender's per-node gauges
	// have. It runs before the loop so a pass that aborts partway leaves the
	// gauges holding only what it managed to observe, never a mix of two passes.
	s.rec.ResetUnusedVolumes()
	for _, f := range findings {
		if ctx.Err() != nil {
			return len(findings), ctx.Err()
		}
		s.report(f)
		outcome, err := s.publish(ctx, f, now)
		if err != nil {
			s.rec.ObserveError("pv_annotate")
			s.logger.Error("failed to annotate object with its unused-volume verdict",
				"kind", f.Kind, "object", f.Key(), "reason", f.Reason, "outcome", "failed", "error", err)
			continue
		}
		s.logger.Debug("unused-volume annotations settled",
			"kind", f.Kind, "object", f.Key(), "unused", f.Unused, "outcome", outcome)
		s.emit(f, outcome)
	}
	s.summarize(findings)
	return len(findings), nil
}

// report records one finding's metrics and logs it. Only a reportable finding
// (unused for at least MinUnusedAge) is exported or logged at info: everything
// below the threshold is mostly workloads between two Pods, and exporting it
// would bury the findings that matter in churn.
func (s *Scanner) report(f Finding) {
	if !f.Reportable {
		return
	}
	age := f.Age.Seconds()
	size := float64(f.CapacityBytes)
	switch f.Kind {
	case KindPVC:
		s.rec.ObserveUnusedPVC(f.Namespace, f.Name, f.VolumeName, f.VolumeID, f.StorageClass, f.Reason, age, size)
	case KindPV:
		s.rec.ObserveUnusedPV(f.Name, f.VolumeID, f.StorageClass, f.Reason, f.ReclaimPolicy, f.ClaimNamespace, f.ClaimName, age, size)
	}
	s.logger.Info("unused volume identified, nothing was deleted",
		"kind", f.Kind, "object", f.Key(), "reason", f.Reason,
		"unused_since", f.UnusedSince.UTC().Format(time.RFC3339),
		"unused_days", int(f.Age/(24*time.Hour)),
		"capacity_bytes", f.CapacityBytes, "storage_class", f.StorageClass,
		"volume_id", f.VolumeID, "bound_to", f.BoundTo(), "reclaim_policy", f.ReclaimPolicy)
}

// emit publishes one finding's Kubernetes Event against the object itself, so the
// verdict is visible in kubectl describe next to the object's own history rather
// than only in the controller's logs.
//
// Two moments are worth an Event, and they are shaped differently. A reported
// finding is a standing state, republished every pass and aggregated by the
// recorder into one Event whose count rises. Coming back into use is a
// transition, published once by the pass that erases the mark and only when that
// pass actually erased something.
//
// A dry run emits nothing: an Event is a cluster write like the annotation it
// accompanies. Findings below minUnusedAge emit nothing either, for the same
// reason they are not exported: a workload between two Pods is not news.
func (s *Scanner) emit(f Finding, outcome string) {
	if s.events == nil || s.dryRun {
		return
	}
	switch {
	case f.Reportable:
		s.eventf(f, corev1.EventTypeNormal, reasonUnusedDetected,
			"Unused for %d days (%s), holding %s. Nothing was deleted. Review and remove it manually if the data is no longer needed.",
			int(f.Age/(24*time.Hour)), f.Reason, describeCapacity(f))
	case !f.Unused && outcome == outcomeWritten:
		s.eventf(f, corev1.EventTypeNormal, reasonUnusedCleared,
			"Back in use (%s). The unused mark has been removed.", f.Reason)
	}
}

// eventf dispatches to the emitter method matching the finding's kind.
func (s *Scanner) eventf(f Finding, eventType, reason, messageFmt string, args ...any) {
	switch f.Kind {
	case KindPVC:
		s.events.ClaimEventf(f.Namespace, f.Name, f.UID, eventType, reason, messageFmt, args...)
	case KindPV:
		s.events.VolumeEventf(f.Name, f.UID, eventType, reason, messageFmt, args...)
	}
}

// describeCapacity renders the finding's size and backing volume for an Event
// message. A claim that never bound has neither, and saying so beats printing a
// zero that reads like a measurement.
func describeCapacity(f Finding) string {
	if f.CapacityBytes == 0 {
		return "no provisioned capacity"
	}
	size := fmt.Sprintf("%dGi", f.CapacityBytes/(1024*1024*1024))
	if f.VolumeID == "" {
		return size
	}
	return size + " on " + f.VolumeID
}

// summarize publishes the per-reason counts and the total capacity they hold,
// then logs the pass. Every known reason is published even when it matched
// nothing, so a query for a reason that has stopped occurring reads as zero
// rather than as no data.
func (s *Scanner) summarize(findings []Finding) {
	counts := map[string]int{}
	bytes := map[string]int64{}
	for _, f := range findings {
		if !f.Reportable {
			continue
		}
		k := f.Kind + "/" + f.Reason
		counts[k]++
		bytes[k] += f.CapacityBytes
	}
	var totalCount int
	var totalBytes int64
	for kind, reasons := range map[string][]string{KindPVC: UnusedPVCReasons, KindPV: UnusedPVReasons} {
		for _, reason := range reasons {
			k := kind + "/" + reason
			s.rec.ObserveUnusedSummary(kind, reason, counts[k], bytes[k])
			totalCount += counts[k]
			totalBytes += bytes[k]
		}
	}
	s.logger.Info("unused volume scan completed",
		"objects_scanned", len(findings),
		"unused_reported", totalCount,
		"unused_capacity_bytes", totalBytes,
		"min_unused_age", s.minUnusedAge.String(),
		"dry_run", s.dryRun)
}

// Outcomes of one object's annotation attempt.
const (
	outcomeWritten   = "written"
	outcomeUnchanged = "unchanged"
	outcomeDryRun    = "dry_run"
)

// publish writes the object's annotations and reports which outcome happened.
// The patch is skipped when nothing changed and unused-observed-at is still
// fresh, which in a healthy cluster is almost every object on almost every pass.
//
// Annotations are written from the first pass that sees an object unused, not
// from the pass that first reports it: the annotation is where the clock lives,
// so waiting for MinUnusedAge to elapse would mean it never does.
func (s *Scanner) publish(ctx context.Context, f Finding, now time.Time) (string, error) {
	desired := buildAnnotations(f)
	if !desired.needsWrite(f.Annotations, now) {
		return outcomeUnchanged, nil
	}
	if len(desired.set) > 0 {
		desired.set[key(keyObservedAt)] = now.UTC().Format(time.RFC3339)
	}
	if s.dryRun {
		return outcomeDryRun, nil
	}
	var err error
	switch f.Kind {
	case KindPVC:
		err = s.kube.AnnotatePVC(ctx, f.Namespace, f.Name, desired.set, desired.remove)
	case KindPV:
		err = s.kube.AnnotatePV(ctx, f.Name, desired.set, desired.remove)
	}
	if err != nil {
		return "", err
	}
	return outcomeWritten, nil
}

// Findings runs one classification pass and returns its results without
// touching the cluster's annotations or the metrics. It is what the CLI
// subcommand reports, so an operator can see the verdict before enabling the
// loop that persists it.
func (s *Scanner) Findings(ctx context.Context) ([]Finding, error) {
	inv, err := s.kube.Inventory(ctx)
	if err != nil {
		return nil, fmt.Errorf("read volume inventory: %w", err)
	}
	return classify(inv, s.minUnusedAge, s.now()), nil
}
