package observability

import (
	"strings"
	"testing"
	"time"

	"github.com/prometheus/client_golang/prometheus/testutil"

	"github.com/younsl/o/box/kubernetes/argocd-promotion-gate/internal/gate"
)

func TestRecordDecision(t *testing.T) {
	m := NewMetrics()
	m.RecordDecision(gate.Decision{Env: "prd", Code: gate.CodeImageTagMismatch, Allowed: false})
	m.RecordDecision(gate.Decision{Env: "prd", Code: gate.CodeImageTagMismatch, Allowed: false})
	m.RecordDecision(gate.Decision{Env: "prd", Code: gate.CodePassed, Allowed: true})

	if got := testutil.ToFloat64(m.Decisions.WithLabelValues("prd", "ImageTagMismatch", "false")); got != 2 {
		t.Errorf("denied ImageTagMismatch count = %v, want 2", got)
	}
	if got := testutil.ToFloat64(m.Decisions.WithLabelValues("prd", "Passed", "true")); got != 1 {
		t.Errorf("passed count = %v, want 1", got)
	}
}

func TestRecordAdmissionAndLookupFailures(t *testing.T) {
	m := NewMetrics()
	m.RecordAdmission("denied")
	m.RecordAdmission("denied")
	m.RecordAdmission("skipped")
	m.RecordLookupFailure("desired_images")

	if got := testutil.ToFloat64(m.AdmissionRequests.WithLabelValues("denied")); got != 2 {
		t.Errorf("denied admissions = %v, want 2", got)
	}
	if got := testutil.ToFloat64(m.AdmissionRequests.WithLabelValues("skipped")); got != 1 {
		t.Errorf("skipped admissions = %v, want 1", got)
	}
	if got := testutil.ToFloat64(m.LookupFailures.WithLabelValues("desired_images")); got != 1 {
		t.Errorf("lookup failures = %v, want 1", got)
	}
}

func TestMetricNamesAreNamespaced(t *testing.T) {
	// A CounterVec with no observation exposes no family, so each one is
	// touched before gathering.
	m := NewMetrics()
	m.RecordDecision(gate.Decision{Env: "prd", Code: gate.CodePassed, Allowed: true})
	m.RecordAdmission("allowed")
	m.RecordLookupFailure("upstream")

	families, err := m.Registry().Gather()
	if err != nil {
		t.Fatalf("Gather() error = %v", err)
	}
	var found []string
	for _, family := range families {
		if strings.HasPrefix(family.GetName(), namespace+"_") {
			found = append(found, family.GetName())
		}
	}
	if len(found) != 3 {
		t.Errorf("gate metric families = %v, want three", found)
	}
}

func TestRegistryIncludesRuntimeCollectors(t *testing.T) {
	families, err := NewMetrics().Registry().Gather()
	if err != nil {
		t.Fatalf("Gather() error = %v", err)
	}
	var hasGo bool
	for _, family := range families {
		if strings.HasPrefix(family.GetName(), "go_") {
			hasGo = true
			break
		}
	}
	if !hasGo {
		t.Error("the registry exposes no go_* metrics, so runtime health is invisible")
	}
}

func TestRegisterCertificateExpiry(t *testing.T) {
	m := NewMetrics()
	expiry := time.Unix(1893456000, 0)
	m.RegisterCertificateExpiry(func() time.Time { return expiry })

	families, err := m.Registry().Gather()
	if err != nil {
		t.Fatalf("Gather() error = %v", err)
	}
	var got float64
	var found bool
	for _, family := range families {
		if family.GetName() != "argocd_promotion_gate_webhook_certificate_expiry_seconds" {
			continue
		}
		found = true
		got = family.GetMetric()[0].GetGauge().GetValue()
	}
	if !found {
		t.Fatal("the certificate expiry gauge was not registered")
	}
	if got != float64(expiry.Unix()) {
		t.Errorf("gauge = %v, want %v", got, float64(expiry.Unix()))
	}
}

func TestRegisterCertificateExpiryReportsZeroWithNothingLoaded(t *testing.T) {
	// Zero rather than a stale reading, so an alert on "expiring soon" fires
	// instead of silently passing when no pair is loaded at all.
	m := NewMetrics()
	m.RegisterCertificateExpiry(func() time.Time { return time.Time{} })

	families, err := m.Registry().Gather()
	if err != nil {
		t.Fatalf("Gather() error = %v", err)
	}
	for _, family := range families {
		if family.GetName() != "argocd_promotion_gate_webhook_certificate_expiry_seconds" {
			continue
		}
		if got := family.GetMetric()[0].GetGauge().GetValue(); got != 0 {
			t.Errorf("gauge = %v with no certificate loaded, want 0", got)
		}
	}
}

func TestObserveDurations(t *testing.T) {
	m := NewMetrics()
	m.ObserveAdmission("denied", 120*time.Millisecond)
	m.ObserveAdmission("denied", 8*time.Millisecond)
	m.ObserveUpstreamLookup("missing", 2*time.Millisecond)
	m.ObserveDesiredImages("error", 3*time.Second)

	if got := testutil.CollectAndCount(m.AdmissionDuration); got != 1 {
		t.Errorf("admission duration series = %d, want 1", got)
	}
	if got := testutil.CollectAndCount(m.UpstreamLookupSeconds); got != 1 {
		t.Errorf("upstream lookup series = %d, want 1", got)
	}
	if got := testutil.CollectAndCount(m.DesiredImagesSeconds); got != 1 {
		t.Errorf("desired images series = %d, want 1", got)
	}

	// The top bucket has to cover the webhook timeout, or a slow request is
	// invisible in exactly the case anybody would go looking for it.
	if last := latencyBuckets[len(latencyBuckets)-1]; last < 5 {
		t.Errorf("largest latency bucket = %v, want at least the webhook timeout of 5s", last)
	}
}

func TestRecordEvent(t *testing.T) {
	m := NewMetrics()
	m.RecordEvent("PromotionBlocked", "Warning")
	m.RecordEvent("PromotionBlocked", "Warning")
	m.RecordEvent("PromotionWarning", "Normal")

	if got := testutil.ToFloat64(m.Events.WithLabelValues("PromotionBlocked", "Warning")); got != 2 {
		t.Errorf("blocked events = %v, want 2", got)
	}
	if got := testutil.ToFloat64(m.Events.WithLabelValues("PromotionWarning", "Normal")); got != 1 {
		t.Errorf("warned events = %v, want 1", got)
	}
}
