// Package observability holds the Prometheus registry, the metric set, and the
// HTTP surfaces that expose /metrics, the health endpoints, and the UI
// extension API.
package observability

import (
	"strconv"
	"time"

	"github.com/prometheus/client_golang/prometheus"
	"github.com/prometheus/client_golang/prometheus/collectors"

	"github.com/younsl/o/box/kubernetes/argocd-promotion-gate/internal/gate"
)

const namespace = "argocd_promotion_gate"

// latencyBuckets resolve the range that matters for an admission webhook.
//
// The two deadlines the gate lives under are the webhook's own timeoutSeconds
// and argocd.timeoutSeconds, both single-digit seconds, so the buckets are
// dense below one second and stop at five. A request slower than the top
// bucket has already been abandoned by the API server, and where exactly it
// landed after that is not worth a series.
var latencyBuckets = []float64{
	0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1, 2, 3, 5,
}

// Metrics is the gate metric set, registered on its own registry so the
// exposed series stay limited to what this binary owns.
//
// Application identity is deliberately absent from the labels: it belongs in
// the logs, where a denial costs one line, not in a time series, where it
// would cost one series per Application forever.
type Metrics struct {
	registry *prometheus.Registry

	Decisions         *prometheus.CounterVec
	AdmissionRequests *prometheus.CounterVec
	LookupFailures    *prometheus.CounterVec
	Events            *prometheus.CounterVec

	AdmissionDuration     *prometheus.HistogramVec
	UpstreamLookupSeconds *prometheus.HistogramVec
	DesiredImagesSeconds  *prometheus.HistogramVec
}

// NewMetrics builds and registers the metric set.
func NewMetrics() *Metrics {
	registry := prometheus.NewRegistry()
	registry.MustRegister(
		collectors.NewGoCollector(),
		collectors.NewProcessCollector(collectors.ProcessCollectorOpts{}),
	)

	m := &Metrics{
		registry: registry,
		Decisions: prometheus.NewCounterVec(prometheus.CounterOpts{
			Namespace: namespace,
			Name:      "decisions_total",
			Help:      "Gate verdicts by environment, reason code, and outcome.",
		}, []string{"env", "code", "allowed"}),
		AdmissionRequests: prometheus.NewCounterVec(prometheus.CounterOpts{
			Namespace: namespace,
			Name:      "admission_requests_total",
			Help:      "Admission requests handled, labeled by outcome.",
		}, []string{"outcome"}),
		LookupFailures: prometheus.NewCounterVec(prometheus.CounterOpts{
			Namespace: namespace,
			Name:      "lookup_failures_total",
			Help:      "Fact lookups that failed, labeled by the kind of lookup.",
		}, []string{"kind"}),
		Events: prometheus.NewCounterVec(prometheus.CounterOpts{
			Namespace: namespace,
			Name:      "events_total",
			Help:      "Kubernetes Events submitted for a verdict, by event reason and type. Verdict codes live on decisions_total.",
		}, []string{"reason", "type"}),
		AdmissionDuration: prometheus.NewHistogramVec(prometheus.HistogramOpts{
			Namespace: namespace,
			Name:      "admission_duration_seconds",
			Help:      "Wall time to answer one admission request, labeled by outcome.",
			Buckets:   latencyBuckets,
		}, []string{"outcome"}),
		UpstreamLookupSeconds: prometheus.NewHistogramVec(prometheus.HistogramOpts{
			Namespace: namespace,
			Name:      "upstream_lookup_duration_seconds",
			Help:      "Wall time of the Kubernetes read of the upstream Application, labeled by result.",
			Buckets:   latencyBuckets,
		}, []string{"result"}),
		DesiredImagesSeconds: prometheus.NewHistogramVec(prometheus.HistogramOpts{
			Namespace: namespace,
			Name:      "desired_images_duration_seconds",
			Help: "Wall time of the desired image lookup, labeled by result. " +
				"A cache hit is served without an argocd-server call, so the distribution is bimodal by design.",
			Buckets: latencyBuckets,
		}, []string{"result"}),
	}

	registry.MustRegister(
		m.Decisions, m.AdmissionRequests, m.LookupFailures, m.Events,
		m.AdmissionDuration, m.UpstreamLookupSeconds, m.DesiredImagesSeconds,
	)
	return m
}

// Registry exposes the registry for the /metrics handler.
func (m *Metrics) Registry() *prometheus.Registry { return m.registry }

// RecordDecision counts one verdict.
func (m *Metrics) RecordDecision(verdict gate.Decision) {
	m.Decisions.WithLabelValues(verdict.Env, string(verdict.Code), strconv.FormatBool(verdict.Allowed)).Inc()
}

// RecordAdmission counts one admission request outcome.
func (m *Metrics) RecordAdmission(outcome string) {
	m.AdmissionRequests.WithLabelValues(outcome).Inc()
}

// RecordLookupFailure counts one failed fact lookup.
func (m *Metrics) RecordLookupFailure(kind string) {
	m.LookupFailures.WithLabelValues(kind).Inc()
}

// RecordEvent counts one Kubernetes Event handed to the broadcaster.
//
// It counts submissions, not writes. The broadcaster delivers asynchronously
// and drops or aggregates on its own, so this is the gate's intent rather than
// what landed in etcd.
func (m *Metrics) RecordEvent(reason, eventType string) {
	m.Events.WithLabelValues(reason, eventType).Inc()
}

// ObserveAdmission records how long one admission request took.
//
// This is the number the webhook's timeoutSeconds has to cover. A denial that
// arrives after the API server gave up is indistinguishable from an outage,
// and with failurePolicy Fail both block the sync for reasons nobody can read.
func (m *Metrics) ObserveAdmission(outcome string, d time.Duration) {
	m.AdmissionDuration.WithLabelValues(outcome).Observe(d.Seconds())
}

// ObserveUpstreamLookup records the Kubernetes read of the upstream Application.
func (m *Metrics) ObserveUpstreamLookup(result string, d time.Duration) {
	m.UpstreamLookupSeconds.WithLabelValues(result).Observe(d.Seconds())
}

// ObserveDesiredImages records the desired image lookup, cache hits included.
func (m *Metrics) ObserveDesiredImages(result string, d time.Duration) {
	m.DesiredImagesSeconds.WithLabelValues(result).Observe(d.Seconds())
}

// RegisterCertificateExpiry publishes the webhook certificate's expiry as a
// unix timestamp, read at scrape time.
//
// It covers only the certificate running out. A re-issued CA leaving the served
// leaf off the published caBundle is invisible from here, because the handshake
// fails before any request reaches this process. That one shows up only as
// apiserver_admission_webhook_rejection_count{error_type="calling_webhook_error"}.
func (m *Metrics) RegisterCertificateExpiry(notAfter func() time.Time) {
	m.registry.MustRegister(prometheus.NewGaugeFunc(prometheus.GaugeOpts{
		Namespace: namespace,
		Name:      "webhook_certificate_expiry_seconds",
		Help:      "Expiry of the serving certificate the webhook currently has loaded, as a unix timestamp.",
	}, func() float64 {
		expiry := notAfter()
		if expiry.IsZero() {
			return 0
		}
		return float64(expiry.Unix())
	}))
}
