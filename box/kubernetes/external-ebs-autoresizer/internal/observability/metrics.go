// Package observability provides the Prometheus metrics registry and health
// endpoints exposed by the long-running process.
package observability

import (
	"context"
	"errors"
	"fmt"
	"net/http"
	"time"

	"github.com/prometheus/client_golang/prometheus"
	"github.com/prometheus/client_golang/prometheus/promhttp"
)

// instanceLabels is the shared identity label set of the per-instance gauges
// (root_usage_percent, root_volume_size_gib). Keeping it identical across both
// gauges lets dashboards join them without relabeling.
var instanceLabels = []string{"instance_id", "device", "volume_id", "name"}

// The unused volume series are split the way Prometheus splits identity from
// measurement: an _info gauge that is always 1 and carries every descriptive
// label, and value gauges keyed by nothing but the object's own name. Putting
// the descriptive labels on the value gauges instead would make a reason change
// (or a rebind) start a new series, so a range query over an object's age would
// break exactly when something about it changed. It also duplicates a long label
// set across three metrics rather than joining it in once.
//
// Both _info sets carry volume_id, which is what turns a row of the report into
// the EC2 volume that is actually billing.
var (
	unusedPVCInfoLabels = []string{"namespace", "name", "volume_name", "volume_id", "storage_class", "reason"}
	unusedPVInfoLabels  = []string{"name", "volume_id", "storage_class", "reason", "reclaim_policy", "claim_namespace", "claim_name"}
	unusedPVCLabels     = []string{"namespace", "name"}
	unusedPVLabels      = []string{"name"}
)

// nodeLabels is the shared identity label set of the per-node throughput gauges.
// It is kept identical across all three so a dashboard can compute headroom
// (recommended minus current) with a plain vector match.
var nodeLabels = []string{"node", "instance_id", "volume_id"}

// Metrics holds the application's Prometheus collectors and implements both
// resizer.Recorder and throughput.Recorder.
type Metrics struct {
	registry        *prometheus.Registry
	usage           *prometheus.GaugeVec
	volumeSize      *prometheus.GaugeVec
	resizeTotal     *prometheus.CounterVec
	skipTotal       *prometheus.CounterVec
	errorTotal      *prometheus.CounterVec
	reconcileTotal  prometheus.Counter
	policyInstances *prometheus.GaugeVec

	nodeCurrentMiBps          *prometheus.GaugeVec
	nodePeakMiBps             *prometheus.GaugeVec
	nodeRecommendedMiBps      *prometheus.GaugeVec
	recommendationTotal       *prometheus.CounterVec
	throughputApplyTotal      *prometheus.CounterVec
	throughputApplySkipTotal  *prometheus.CounterVec
	recommenderReconcileTotal prometheus.Counter

	unusedPVCInfo       *prometheus.GaugeVec
	unusedPVInfo        *prometheus.GaugeVec
	unusedPVCAge        *prometheus.GaugeVec
	unusedPVCCapacity   *prometheus.GaugeVec
	unusedPVAge         *prometheus.GaugeVec
	unusedPVCapacity    *prometheus.GaugeVec
	unusedTotal         *prometheus.GaugeVec
	unusedCapacityTotal *prometheus.GaugeVec
	unusedScanTotal     prometheus.Counter
}

// NewMetrics builds the collectors and registers them on a private registry.
func NewMetrics() *Metrics {
	m := &Metrics{
		registry: prometheus.NewRegistry(),
		usage: prometheus.NewGaugeVec(prometheus.GaugeOpts{
			Name: "external_ebs_autoresizer_root_usage_percent",
			Help: "Most recently measured root filesystem usage percent per instance.",
		}, instanceLabels),
		volumeSize: prometheus.NewGaugeVec(prometheus.GaugeOpts{
			Name: "external_ebs_autoresizer_root_volume_size_gib",
			Help: "Most recently observed root EBS volume size in GiB per instance. Size is a gauge value, not a label, so the series identity survives resizes.",
		}, instanceLabels),
		resizeTotal: prometheus.NewCounterVec(prometheus.CounterOpts{
			Name: "external_ebs_autoresizer_resize_total",
			Help: "Total resize attempts by result and matched resize policy.",
		}, []string{"result", "policy"}),
		skipTotal: prometheus.NewCounterVec(prometheus.CounterOpts{
			Name: "external_ebs_autoresizer_skip_total",
			Help: "Total instances skipped without a resize attempt, by reason and matched resize policy.",
		}, []string{"reason", "policy"}),
		errorTotal: prometheus.NewCounterVec(prometheus.CounterOpts{
			Name: "external_ebs_autoresizer_error_total",
			Help: "Total errors by reconcile stage.",
		}, []string{"stage"}),
		reconcileTotal: prometheus.NewCounter(prometheus.CounterOpts{
			Name: "external_ebs_autoresizer_reconcile_total",
			Help: "Total reconcile passes started.",
		}),
		policyInstances: prometheus.NewGaugeVec(prometheus.GaugeOpts{
			Name: "external_ebs_autoresizer_policy_instances",
			Help: "Number of discovered instances matched by each resize policy in the latest pass (policy=default for instances matching no named policy).",
		}, []string{"policy"}),
		nodeCurrentMiBps: prometheus.NewGaugeVec(prometheus.GaugeOpts{
			Name: "external_ebs_autoresizer_node_throughput_current_mibps",
			Help: "Provisioned EBS throughput in MiB/s of the volume attached to each Kubernetes node.",
		}, nodeLabels),
		nodePeakMiBps: prometheus.NewGaugeVec(prometheus.GaugeOpts{
			Name: "external_ebs_autoresizer_node_throughput_observed_peak_mibps",
			Help: "Observed peak EBS throughput in MiB/s per Kubernetes node over the configured observation window.",
		}, nodeLabels),
		nodeRecommendedMiBps: prometheus.NewGaugeVec(prometheus.GaugeOpts{
			Name: "external_ebs_autoresizer_node_throughput_recommended_mibps",
			Help: "Recommended EBS throughput in MiB/s per Kubernetes node. Equal to the current value when no change is recommended.",
		}, nodeLabels),
		recommendationTotal: prometheus.NewCounterVec(prometheus.CounterOpts{
			Name: "external_ebs_autoresizer_recommendation_total",
			Help: "Total throughput recommendations published, by action (increase, decrease, none, unknown) and reason.",
		}, []string{"action", "reason"}),
		throughputApplyTotal: prometheus.NewCounterVec(prometheus.CounterOpts{
			Name: "external_ebs_autoresizer_throughput_apply_total",
			Help: "Total throughput piggyback attempts on volume size modifications, by result (applied, fallback_size_only).",
		}, []string{"result"}),
		throughputApplySkipTotal: prometheus.NewCounterVec(prometheus.CounterOpts{
			Name: "external_ebs_autoresizer_throughput_apply_skip_total",
			Help: "Total volume size modifications that carried no throughput piggyback, by reason (no_recommendation, stale, not_increase).",
		}, []string{"reason"}),
		recommenderReconcileTotal: prometheus.NewCounter(prometheus.CounterOpts{
			Name: "external_ebs_autoresizer_recommender_reconcile_total",
			Help: "Total throughput recommender passes started.",
		}),
		unusedPVCInfo: prometheus.NewGaugeVec(prometheus.GaugeOpts{
			Name: "external_ebs_autoresizer_unused_pvc_info",
			Help: "Always 1, one series per reported unused PersistentVolumeClaim. Carries the descriptive labels the report is listed by (namespace, name, volume_name, volume_id, storage_class, reason); join it to unused_pvc_age_seconds and unused_pvc_capacity_bytes on namespace and name for the numbers.",
		}, unusedPVCInfoLabels),
		unusedPVInfo: prometheus.NewGaugeVec(prometheus.GaugeOpts{
			Name: "external_ebs_autoresizer_unused_pv_info",
			Help: "Always 1, one series per reported unused PersistentVolume. Carries the descriptive labels the report is listed by (name, volume_id, storage_class, reason, reclaim_policy, claim_namespace, claim_name); join it to unused_pv_age_seconds and unused_pv_capacity_bytes on name for the numbers.",
		}, unusedPVInfoLabels),
		unusedPVCAge: prometheus.NewGaugeVec(prometheus.GaugeOpts{
			Name: "external_ebs_autoresizer_unused_pvc_age_seconds",
			Help: "How long each reported PersistentVolumeClaim has been continuously unused, in seconds. Only claims past unusedVolumeScan.minUnusedAge are exported. Descriptive labels live on unused_pvc_info.",
		}, unusedPVCLabels),
		unusedPVCCapacity: prometheus.NewGaugeVec(prometheus.GaugeOpts{
			Name: "external_ebs_autoresizer_unused_pvc_capacity_bytes",
			Help: "Provisioned capacity in bytes of each reported unused PersistentVolumeClaim. Keyed identically to the age gauge so the two join without relabeling.",
		}, unusedPVCLabels),
		unusedPVAge: prometheus.NewGaugeVec(prometheus.GaugeOpts{
			Name: "external_ebs_autoresizer_unused_pv_age_seconds",
			Help: "How long each reported PersistentVolume has been continuously unused, in seconds. Descriptive labels live on unused_pv_info.",
		}, unusedPVLabels),
		unusedPVCapacity: prometheus.NewGaugeVec(prometheus.GaugeOpts{
			Name: "external_ebs_autoresizer_unused_pv_capacity_bytes",
			Help: "Provisioned capacity in bytes of each reported unused PersistentVolume. Keyed identically to the age gauge so the two join without relabeling.",
		}, unusedPVLabels),
		unusedTotal: prometheus.NewGaugeVec(prometheus.GaugeOpts{
			Name: "external_ebs_autoresizer_unused_objects",
			Help: "Number of reported unused objects in the latest scan, by kind and reason. Every known reason is published on every pass, so a reason that has stopped occurring reads as zero rather than as no data.",
		}, []string{"kind", "reason"}),
		unusedCapacityTotal: prometheus.NewGaugeVec(prometheus.GaugeOpts{
			Name: "external_ebs_autoresizer_unused_objects_capacity_bytes",
			Help: "Total provisioned capacity in bytes held by reported unused objects in the latest scan, by kind and reason.",
		}, []string{"kind", "reason"}),
		unusedScanTotal: prometheus.NewCounter(prometheus.CounterOpts{
			Name: "external_ebs_autoresizer_unused_scan_total",
			Help: "Total unused volume scan passes started.",
		}),
	}
	m.registry.MustRegister(m.usage, m.volumeSize, m.resizeTotal, m.skipTotal, m.errorTotal, m.reconcileTotal, m.policyInstances,
		m.nodeCurrentMiBps, m.nodePeakMiBps, m.nodeRecommendedMiBps, m.recommendationTotal, m.throughputApplyTotal,
		m.throughputApplySkipTotal, m.recommenderReconcileTotal,
		m.unusedPVCInfo, m.unusedPVInfo,
		m.unusedPVCAge, m.unusedPVCCapacity, m.unusedPVAge, m.unusedPVCapacity,
		m.unusedTotal, m.unusedCapacityTotal, m.unusedScanTotal)
	return m
}

// ResetNodeThroughput clears the per-node throughput gauges at the start of a
// recommender pass. Nodes are short-lived under Karpenter, and without the reset
// a terminated node's last reading would stay exported forever and read as a
// live node with stale numbers.
func (m *Metrics) ResetNodeThroughput() {
	m.nodeCurrentMiBps.Reset()
	m.nodePeakMiBps.Reset()
	m.nodeRecommendedMiBps.Reset()
}

// ObserveNodeThroughput records one node's provisioned, observed, and recommended
// throughput.
func (m *Metrics) ObserveNodeThroughput(node, instanceID, volumeID string, currentMiBps, peakMiBps, recommendedMiBps float64) {
	m.nodeCurrentMiBps.WithLabelValues(node, instanceID, volumeID).Set(currentMiBps)
	m.nodePeakMiBps.WithLabelValues(node, instanceID, volumeID).Set(peakMiBps)
	m.nodeRecommendedMiBps.WithLabelValues(node, instanceID, volumeID).Set(recommendedMiBps)
}

// ObserveRecommendation counts one published recommendation by action and reason.
func (m *Metrics) ObserveRecommendation(action, reason string) {
	m.recommendationTotal.WithLabelValues(action, reason).Inc()
}

// ObserveUsage records the latest measured usage for an instance.
func (m *Metrics) ObserveUsage(instanceID, device, volumeID, name string, percent float64) {
	m.usage.WithLabelValues(instanceID, device, volumeID, name).Set(percent)
}

// ObserveVolumeSize records the latest known root volume size for an instance.
// The identity labels match ObserveUsage so the two gauges join cleanly.
func (m *Metrics) ObserveVolumeSize(instanceID, device, volumeID, name string, sizeGiB int32) {
	m.volumeSize.WithLabelValues(instanceID, device, volumeID, name).Set(float64(sizeGiB))
}

// ObserveResize counts a resize attempt by outcome and matched policy.
func (m *Metrics) ObserveResize(success bool, policy string) {
	result := "failure"
	if success {
		result = "success"
	}
	m.resizeTotal.WithLabelValues(result, policy).Inc()
}

// ObserveSkip counts an instance skipped without a resize attempt. reason is
// one of: below_threshold, max_size, cooldown, dry_run. policy is the matched
// resize policy.
func (m *Metrics) ObserveSkip(reason, policy string) {
	m.skipTotal.WithLabelValues(reason, policy).Inc()
}

// ObservePolicyInstances records, per resize policy, how many discovered
// instances matched it in the latest reconcile pass. Policies that matched
// nothing this pass are set to 0 so stale counts do not linger.
func (m *Metrics) ObservePolicyInstances(counts map[string]int) {
	m.policyInstances.Reset()
	for policy, n := range counts {
		m.policyInstances.WithLabelValues(policy).Set(float64(n))
	}
}

// ObserveThroughputApply counts one attempted throughput piggyback: applied,
// or fallback_size_only when the combined request was rejected. Skips are a
// separate metric (ObserveThroughputApplySkip) because they are a different
// unit of work: no combined request was ever sent, and mixing the two would
// make either metric's sum meaningless.
func (m *Metrics) ObserveThroughputApply(result string) {
	m.throughputApplyTotal.WithLabelValues(result).Inc()
}

// ObserveThroughputApplySkip counts one volume modification that proceeded
// without a throughput piggyback, by reason. The caller supplies values from a
// fixed set, so the series count stays constant regardless of fleet size.
func (m *Metrics) ObserveThroughputApplySkip(reason string) {
	m.throughputApplySkipTotal.WithLabelValues(reason).Inc()
}

// ObserveRecommenderReconcile counts a recommender pass start, the liveness
// signal of the recommender loop the way reconcile_total is for the resizer.
func (m *Metrics) ObserveRecommenderReconcile() {
	m.recommenderReconcileTotal.Inc()
}

// ResetUnusedVolumes clears every unused volume gauge at the start of a scan
// pass. Claims and volumes are deleted precisely because someone acted on this
// report, and without the reset a deleted object's last reading would stay
// exported forever and read as one still waiting to be cleaned up.
func (m *Metrics) ResetUnusedVolumes() {
	m.unusedPVCInfo.Reset()
	m.unusedPVInfo.Reset()
	m.unusedPVCAge.Reset()
	m.unusedPVCCapacity.Reset()
	m.unusedPVAge.Reset()
	m.unusedPVCapacity.Reset()
	m.unusedTotal.Reset()
	m.unusedCapacityTotal.Reset()
}

// ObserveUnusedPVC records one reported unused PersistentVolumeClaim: its
// descriptive labels on the _info series, its age and capacity on the value
// series keyed by namespace and name.
func (m *Metrics) ObserveUnusedPVC(namespace, name, volumeName, volumeID, storageClass, reason string, ageSeconds, capacityBytes float64) {
	m.unusedPVCInfo.WithLabelValues(namespace, name, volumeName, volumeID, storageClass, reason).Set(1)
	m.unusedPVCAge.WithLabelValues(namespace, name).Set(ageSeconds)
	m.unusedPVCCapacity.WithLabelValues(namespace, name).Set(capacityBytes)
}

// ObserveUnusedPV records one reported unused PersistentVolume. claimNamespace
// and claimName are the claim it is (or was) bound to, empty for a volume that
// was never claimed. They are what lets a report listed by volume name still
// name the workload that left it behind.
func (m *Metrics) ObserveUnusedPV(name, volumeID, storageClass, reason, reclaimPolicy, claimNamespace, claimName string, ageSeconds, capacityBytes float64) {
	m.unusedPVInfo.WithLabelValues(name, volumeID, storageClass, reason, reclaimPolicy, claimNamespace, claimName).Set(1)
	m.unusedPVAge.WithLabelValues(name).Set(ageSeconds)
	m.unusedPVCapacity.WithLabelValues(name).Set(capacityBytes)
}

// ObserveUnusedSummary records the count and total capacity of one kind and
// reason. It is the low-cardinality view the per-object gauges are too wide to
// alert on.
func (m *Metrics) ObserveUnusedSummary(kind, reason string, count int, capacityBytes int64) {
	m.unusedTotal.WithLabelValues(kind, reason).Set(float64(count))
	m.unusedCapacityTotal.WithLabelValues(kind, reason).Set(float64(capacityBytes))
}

// ObserveUnusedScan counts a scan pass start, the liveness signal of the scanner
// loop the way reconcile_total is for the resizer.
func (m *Metrics) ObserveUnusedScan() {
	m.unusedScanTotal.Inc()
}

// ObserveError counts an error in the given reconcile stage.
func (m *Metrics) ObserveError(stage string) {
	m.errorTotal.WithLabelValues(stage).Inc()
}

// ObserveReconcile counts a reconcile pass start.
func (m *Metrics) ObserveReconcile() {
	m.reconcileTotal.Inc()
}

// Serve runs the /metrics HTTP server until ctx is cancelled.
func (m *Metrics) Serve(ctx context.Context, port int) error {
	mux := http.NewServeMux()
	mux.Handle("/metrics", promhttp.HandlerFor(m.registry, promhttp.HandlerOpts{}))
	return serveUntilDone(ctx, port, mux)
}

func serveUntilDone(ctx context.Context, port int, handler http.Handler) error {
	srv := &http.Server{
		Addr:              fmt.Sprintf(":%d", port),
		Handler:           handler,
		ReadHeaderTimeout: 5 * time.Second,
	}
	errCh := make(chan error, 1)
	go func() {
		if err := srv.ListenAndServe(); err != nil && !errors.Is(err, http.ErrServerClosed) {
			errCh <- err
		}
	}()
	select {
	case <-ctx.Done():
		shutdownCtx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
		defer cancel()
		return srv.Shutdown(shutdownCtx)
	case err := <-errCh:
		return err
	}
}
