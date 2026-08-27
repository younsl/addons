package main

import (
	"log/slog"

	"github.com/younsl/o/box/kubernetes/external-ebs-autoresizer/internal/annotations"
	"github.com/younsl/o/box/kubernetes/external-ebs-autoresizer/internal/config"
	"github.com/younsl/o/box/kubernetes/external-ebs-autoresizer/internal/pvscan"
)

// This file wires the unused volume scanner, which is independent of both the
// resize loop and the throughput recommender: it reads only the Kubernetes API,
// never AWS, and its only writes are an annotation and an Event recording what
// it found.

// logUnusedVolumeScanPolicy logs the scanner's effective settings at INFO, next
// to the resize and piggyback policy lines, so what the scan covers and how often
// it runs is unambiguous in the Pod's startup logs.
//
// It exists precisely because the scanner has no configuration surface. Every
// value here is a constant, so an operator reading the mounted config file finds
// nothing about it at all, and "which numbers is it actually using" would
// otherwise only be answerable by reading the source. Whether the loop then
// really starts is reported separately by buildScanner, which is the only place
// that knows.
func logUnusedVolumeScanPolicy(logger *slog.Logger, cfg *config.Config) {
	logger.Info("Unused volume scan is enabled and will identify PersistentVolumeClaims and PersistentVolumes that no workload is using, publishing each verdict as object annotations, Kubernetes Events, and Prometheus metrics. It never deletes a claim or a volume",
		"enabled", true,
		"configurable", false,
		"interval", pvscan.Interval.String(),
		"min_unused_age", pvscan.MinUnusedAge.String(),
		"namespace_scope", "all",
		"reads", []string{"persistentvolumeclaims", "persistentvolumes", "pods", "statefulsets"},
		"writes", []string{"annotations", "events"},
		"annotation_prefix", annotations.Prefix,
		"event_reasons", []string{"UnusedVolumeDetected", "UnusedVolumeCleared"},
		"dry_run", cfg.DryRun)
}

// buildScanner constructs the scanner, or returns nil when it cannot run. The
// scanner has no enable switch: it never mutates a claim or a volume, its cost is
// four list calls an hour, and an addon that already runs inside the cluster is
// the natural place for the report. A nil return is not an error either, so a
// process without in-cluster access (running the binary locally) still runs the
// resize loop.
func buildScanner(cfg *config.Config, rec pvscan.Recorder, events pvscan.EventEmitter, logger *slog.Logger) *pvscan.Scanner {
	kube, err := pvscan.NewClient()
	if err != nil {
		logger.Error("Unused volume scan will not run: no in-cluster Kubernetes access. The resize loop is unaffected", "error", err)
		return nil
	}
	logger.Info("Unused volume scan loop ready", "events", events != nil, "dry_run", cfg.DryRun)
	return pvscan.New(cfg.DryRun, kube, rec, events, logger)
}
