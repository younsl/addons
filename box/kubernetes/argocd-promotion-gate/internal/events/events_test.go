package events

import (
	"bytes"
	"context"
	"errors"
	"io"
	"log/slog"
	"strings"
	"sync"
	"testing"
	"time"

	corev1 "k8s.io/api/core/v1"
	apierrors "k8s.io/apimachinery/pkg/api/errors"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"k8s.io/apimachinery/pkg/runtime"
	"k8s.io/apimachinery/pkg/runtime/schema"
	"k8s.io/client-go/kubernetes/fake"
	k8stesting "k8s.io/client-go/testing"

	"github.com/younsl/o/box/kubernetes/argocd-promotion-gate/internal/gate"
	"github.com/younsl/o/box/kubernetes/argocd-promotion-gate/internal/observability"
)

func discardLogger() *slog.Logger {
	return slog.New(slog.NewTextHandler(io.Discard, nil))
}

// TestDescribeSeparatesBlockedFromWarned pins the contract the docs promise:
// one reason finds everything the gate refused, whatever the underlying code
// was, and a verdict that simply passed writes nothing at all.
func TestDescribeSeparatesBlockedFromWarned(t *testing.T) {
	tests := []struct {
		name       string
		verdict    gate.Decision
		wantEvent  bool
		wantType   string
		wantReason string
	}{
		{
			name:       "a denial is a warning",
			verdict:    gate.Decision{Allowed: false, Code: gate.CodeUpstreamOutOfSync, Message: "blocked"},
			wantEvent:  true,
			wantType:   corev1.EventTypeWarning,
			wantReason: ReasonBlocked,
		},
		{
			name: "a mismatch allowed by warn mode is normal",
			verdict: gate.Decision{
				Allowed: true, Code: gate.CodeImageTagMismatch,
				Message: "allowed with a warning", Warnings: []string{"tag differs"},
			},
			wantEvent:  true,
			wantType:   corev1.EventTypeNormal,
			wantReason: ReasonWarned,
		},
		{
			name:      "a plain allow is not worth an event",
			verdict:   gate.Decision{Allowed: true, Code: gate.CodePassed, Message: "allowed"},
			wantEvent: false,
		},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			eventType, reason, _, ok := describe(tt.verdict)
			if ok != tt.wantEvent {
				t.Fatalf("describe() emitted = %v, want %v", ok, tt.wantEvent)
			}
			if !ok {
				return
			}
			if eventType != tt.wantType {
				t.Errorf("type = %q, want %q", eventType, tt.wantType)
			}
			if reason != tt.wantReason {
				t.Errorf("reason = %q, want %q", reason, tt.wantReason)
			}
		})
	}
}

// TestEveryCodeCollapsesOntoTwoReasons is the point of the change: an operator
// finds every refusal with one field selector instead of having to know the
// whole set of verdict codes.
func TestEveryCodeCollapsesOntoTwoReasons(t *testing.T) {
	codes := []gate.Code{
		gate.CodeUpstreamOutOfSync, gate.CodeUpstreamUnhealthy,
		gate.CodeImageTagMismatch, gate.CodeLookupFailed, gate.CodePassed,
	}

	for _, code := range codes {
		_, reason, _, ok := describe(gate.Decision{Allowed: false, Code: code, Message: "blocked"})
		if !ok || reason != ReasonBlocked {
			t.Errorf("blocked %s produced reason %q, want %q", code, reason, ReasonBlocked)
		}
		_, reason, _, ok = describe(gate.Decision{
			Allowed: true, Code: code, Message: "warned", Warnings: []string{"w"}})
		if !ok || reason != ReasonWarned {
			t.Errorf("warned %s produced reason %q, want %q", code, reason, ReasonWarned)
		}
	}

	// The quiet path stays quiet for every code, including the ones that can
	// also produce an event when they carry a warning.
	for _, code := range codes {
		if _, _, _, ok := describe(gate.Decision{Allowed: true, Code: code, Message: "allowed"}); ok {
			t.Errorf("a plain allow of %s wrote an event", code)
		}
	}
}

// TestTruncateStaysWithinCapAndExplainsItself guards the size bound and the
// house rule that a cut is announced in words rather than with an ellipsis.
func TestTruncateStaysWithinCapAndExplainsItself(t *testing.T) {
	long := strings.Repeat("a", maxMessageBytes*2)
	got := truncate(long)

	if len(got) > maxMessageBytes {
		t.Errorf("truncated length = %d, want at most %d", len(got), maxMessageBytes)
	}
	if !strings.HasSuffix(got, "too long.") {
		t.Errorf("truncated message does not end by saying it was cut: %q", got[len(got)-40:])
	}
	if short := "already short"; truncate(short) != short {
		t.Errorf("truncate() rewrote a message that fit")
	}
}

// TestEventTextIsPlainProse keeps punctuation that stands in for words out of
// anything a person reads. An event travels through kubectl, the Argo CD UI,
// and whatever ships it onward, and each renders these differently.
func TestEventTextIsPlainProse(t *testing.T) {
	banned := map[string]string{
		"em dash":    "—",
		"en dash":    "–",
		"middle dot": "·",
		"bullet":     "•",
		"ellipsis":   "…",
		"semicolon":  ";",
	}

	subjects := []string{
		truncate(strings.Repeat("a", maxMessageBytes*2)),
	}
	for _, verdict := range []gate.Decision{
		{Allowed: false, Code: gate.CodeUpstreamUnhealthy, Message: "blocked"},
		{Allowed: true, Code: gate.CodeImageTagMismatch, Message: "warned", Warnings: []string{"w"}},
	} {
		_, _, message, ok := describe(verdict)
		if ok {
			subjects = append(subjects, message)
		}
	}

	for _, subject := range subjects {
		for name, char := range banned {
			if strings.Contains(subject, char) {
				t.Errorf("event text contains a %s: %q", name, subject)
			}
		}
	}
}

func TestRecorderWritesWarningEventForDenial(t *testing.T) {
	client := fake.NewClientset()
	metrics := observability.NewMetrics()
	recorder := NewRecorder(client, "argocd", metrics, discardLogger())
	defer recorder.Shutdown()

	recorder.Emit("argocd", "prd-payment-api", "uid-42", gate.Decision{
		Allowed: false,
		Code:    gate.CodeUpstreamOutOfSync,
		Message: "Sync of prd-payment-api is blocked.",
	})

	event := waitForEvent(t, client, "argocd")
	if event.Type != corev1.EventTypeWarning {
		t.Errorf("type = %q, want Warning", event.Type)
	}
	if event.Reason != ReasonBlocked {
		t.Errorf("reason = %q, want %q", event.Reason, ReasonBlocked)
	}
	if event.InvolvedObject.Kind != applicationKind || event.InvolvedObject.Name != "prd-payment-api" {
		t.Errorf("involvedObject = %+v, want the Application it judged", event.InvolvedObject)
	}
	// The Application's own UID, not the admission request's, or nothing
	// linking the event to the object survives.
	if string(event.InvolvedObject.UID) != "uid-42" {
		t.Errorf("involvedObject.uid = %q, want uid-42", event.InvolvedObject.UID)
	}
	if event.Source.Component != component {
		t.Errorf("source = %q, want %q", event.Source.Component, component)
	}
}

func TestRecorderWritesNothingForAPlainAllow(t *testing.T) {
	client := fake.NewClientset()
	metrics := observability.NewMetrics()
	recorder := NewRecorder(client, "argocd", metrics, discardLogger())
	defer recorder.Shutdown()

	recorder.Emit("argocd", "prd-payment-api", "uid-42", gate.Decision{
		Allowed: true,
		Code:    gate.CodePassed,
		Message: "Sync of prd-payment-api is allowed.",
	})

	// Nothing to wait for, so the check is that nothing appears while the
	// broadcaster has had a chance to run.
	time.Sleep(50 * time.Millisecond)
	list, err := client.CoreV1().Events("argocd").List(context.Background(), metav1.ListOptions{})
	if err != nil {
		t.Fatalf("List() error = %v", err)
	}
	if len(list.Items) != 0 {
		t.Errorf("a passing verdict wrote %d events, want none", len(list.Items))
	}
}

// waitForEvent polls because the broadcaster delivers on its own goroutine,
// which is exactly the property that keeps Emit off the admission critical path.
func waitForEvent(t *testing.T, client *fake.Clientset, namespace string) corev1.Event {
	t.Helper()
	deadline := time.Now().Add(5 * time.Second)
	for time.Now().Before(deadline) {
		list, err := client.CoreV1().Events(namespace).List(context.Background(), metav1.ListOptions{})
		if err != nil {
			t.Fatalf("List() error = %v", err)
		}
		if len(list.Items) > 0 {
			return list.Items[0]
		}
		time.Sleep(10 * time.Millisecond)
	}
	t.Fatal("no event was written before the deadline")
	return corev1.Event{}
}

// syncBuffer collects log output written from the broadcaster goroutine.
type syncBuffer struct {
	mu  sync.Mutex
	buf bytes.Buffer
}

func (s *syncBuffer) Write(p []byte) (int, error) {
	s.mu.Lock()
	defer s.mu.Unlock()
	return s.buf.Write(p)
}

func (s *syncBuffer) String() string {
	s.mu.Lock()
	defer s.mu.Unlock()
	return s.buf.String()
}

func waitForLog(t *testing.T, out *syncBuffer, want string) string {
	t.Helper()
	deadline := time.Now().Add(5 * time.Second)
	for time.Now().Before(deadline) {
		if got := out.String(); strings.Contains(got, want) {
			return got
		}
		time.Sleep(10 * time.Millisecond)
	}
	t.Fatalf("log never contained %q, got: %s", want, out.String())
	return ""
}

// TestRecorderLogsASuccessfulWrite covers the only confirmation that a reason
// actually reached the Application rather than being queued and dropped.
func TestRecorderLogsASuccessfulWrite(t *testing.T) {
	out := &syncBuffer{}
	logger := slog.New(slog.NewTextHandler(out, &slog.HandlerOptions{Level: slog.LevelDebug}))
	recorder := NewRecorder(fake.NewClientset(), "argocd", observability.NewMetrics(), logger)
	defer recorder.Shutdown()

	recorder.Emit("argocd", "prd-payment-api", "uid-42", gate.Decision{
		Allowed: false,
		Code:    gate.CodeUpstreamOutOfSync,
		Message: "Sync of prd-payment-api is blocked.",
	})

	got := waitForLog(t, out, "wrote a promotion gate event")
	for _, want := range []string{"prd-payment-api", ReasonBlocked, "Warning"} {
		if !strings.Contains(got, want) {
			t.Errorf("success log does not name %q: %s", want, got)
		}
	}
}

// TestRecorderLogsAFailedWrite is the case a missing RBAC rule produces. It has
// to be visible in the gate's own logger rather than only in a klog line from
// inside client-go.
func TestRecorderLogsAFailedWrite(t *testing.T) {
	client := fake.NewClientset()
	client.PrependReactor("create", "events", func(k8stesting.Action) (bool, runtime.Object, error) {
		return true, nil, apierrors.NewForbidden(
			schema.GroupResource{Resource: "events"}, "", errors.New("no rbac rule"))
	})

	out := &syncBuffer{}
	logger := slog.New(slog.NewTextHandler(out, &slog.HandlerOptions{Level: slog.LevelDebug}))
	recorder := NewRecorder(client, "argocd", observability.NewMetrics(), logger)
	defer recorder.Shutdown()

	recorder.Emit("argocd", "prd-payment-api", "uid-42", gate.Decision{
		Allowed: false,
		Code:    gate.CodeUpstreamOutOfSync,
		Message: "Sync of prd-payment-api is blocked.",
	})

	got := waitForLog(t, out, "could not write a promotion gate event")
	if !strings.Contains(got, "The verdict itself was unaffected") {
		t.Errorf("failure log does not say the verdict stands: %s", got)
	}
}

// TestRecorderLogsWhenThereIsNothingToRecord keeps the quiet path traceable, so
// a missing event can be told apart from a dropped one.
func TestRecorderLogsWhenThereIsNothingToRecord(t *testing.T) {
	out := &syncBuffer{}
	logger := slog.New(slog.NewTextHandler(out, &slog.HandlerOptions{Level: slog.LevelDebug}))
	recorder := NewRecorder(fake.NewClientset(), "argocd", observability.NewMetrics(), logger)
	defer recorder.Shutdown()

	recorder.Emit("argocd", "prd-payment-api", "uid-42", gate.Decision{
		Allowed: true, Code: gate.CodePassed, Message: "allowed",
	})

	if got := out.String(); !strings.Contains(got, "no kubernetes event for this verdict") {
		t.Errorf("the skipped path logged nothing: %s", got)
	}
}
