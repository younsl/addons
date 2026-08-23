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
	}

	registry.MustRegister(m.Decisions, m.AdmissionRequests, m.LookupFailures)
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
