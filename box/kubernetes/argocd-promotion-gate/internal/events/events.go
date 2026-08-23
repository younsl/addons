// Package events records gate verdicts as Kubernetes Events on the
// Application they were about.
//
// The denial message already reaches whoever pressed Sync, through the Argo CD
// error toast. It does not reach anybody looking at the Application afterwards:
// the toast is gone, the webhook's own logs are in another namespace, and the
// Application itself carries no trace of having been refused. An Event closes
// that gap, so `kubectl describe application prd-payment-api` answers "why did
// this not deploy" without anyone needing access to the gate.
package events

import (
	"context"
	"fmt"
	"log/slog"

	corev1 "k8s.io/api/core/v1"
	"k8s.io/apimachinery/pkg/types"
	"k8s.io/client-go/kubernetes"
	"k8s.io/client-go/kubernetes/scheme"
	typedcorev1 "k8s.io/client-go/kubernetes/typed/core/v1"
	"k8s.io/client-go/tools/record"

	"github.com/younsl/o/box/kubernetes/argocd-promotion-gate/internal/gate"
	"github.com/younsl/o/box/kubernetes/argocd-promotion-gate/internal/observability"
)

// component is the Event source, so a reader can tell the gate's events apart
// from the application controller's on the same Application.
const component = "argocd-promotion-gate"

// applicationAPIVersion and applicationKind address the Application an Event
// hangs off. They are written out rather than resolved through a scheme
// because the gate reads Applications unstructured and never registers a type.
const (
	applicationAPIVersion = "argoproj.io/v1alpha1"
	applicationKind       = "Application"
)

// The two reasons an Event from this gate can carry.
//
// A Kubernetes Event reason names what happened, which is why upstream uses
// FailedScheduling and BackOff rather than a state. The verdict code is a
// state, so it stays where a state belongs: the decisions_total label, the
// JSON the panel reads, and the log line. Two reasons also means an operator
// needs one field selector to find everything the gate refused rather than
// having to know the whole set of codes, and the codes are worth little on an
// object the API server deletes an hour later.
const (
	// ReasonBlocked marks a sync the gate refused.
	ReasonBlocked = "PromotionBlocked"
	// ReasonWarned marks a sync the gate allowed while recording something
	// about it.
	ReasonWarned = "PromotionWarning"
)

// maxMessageBytes caps the Event message.
//
// The verdict messages are written as prose for the Argo CD toast and run to a
// few hundred bytes. The cap is not about those; it is about never handing the
// API server an object whose size depends on how many images an Application
// happens to declare.
const maxMessageBytes = 1024

// Emitter records a verdict. The admission handler holds one, or nil when
// event emission is switched off.
type Emitter interface {
	// Emit records verdict against the named Application. It must not block:
	// the caller is on the admission path.
	Emit(namespace, name, uid string, verdict gate.Decision)
}

// Recorder is the Kubernetes-backed Emitter.
type Recorder struct {
	broadcaster record.EventBroadcaster
	recorder    record.EventRecorder
	metrics     *observability.Metrics
	logger      *slog.Logger
}

// NewRecorder builds a Recorder writing Events into namespace.
//
// Delivery is asynchronous: client-go's broadcaster owns a queue, a retry
// loop, and a per-object spam filter, which is why Emit can be called straight
// from the admission path. It also means an Event is best effort. A verdict is
// never withheld because its Event could not be written.
func NewRecorder(client kubernetes.Interface, namespace string, metrics *observability.Metrics, logger *slog.Logger) *Recorder {
	broadcaster := record.NewBroadcaster()
	broadcaster.StartLogging(func(format string, args ...any) {
		logger.Debug(fmt.Sprintf(format, args...))
	})
	broadcaster.StartRecordingToSink(&loggingSink{
		inner:  client.CoreV1().Events(namespace),
		logger: logger,
	})
	return &Recorder{
		broadcaster: broadcaster,
		recorder:    broadcaster.NewRecorder(scheme.Scheme, corev1.EventSource{Component: component}),
		metrics:     metrics,
		logger:      logger,
	}
}

// Emit records one verdict, or nothing when the verdict is unremarkable.
//
// A plain allow produces no Event on purpose. Most syncs pass, and an Event per
// pass would bury the denials it is meant to surface and cost a write on the
// admission path for nothing.
func (r *Recorder) Emit(namespace, name, uid string, verdict gate.Decision) {
	eventType, reason, message, ok := describe(verdict)
	if !ok {
		r.logger.Debug("no kubernetes event for this verdict",
			"app", name,
			"namespace", namespace,
			"code", string(verdict.Code),
			"allowed", verdict.Allowed,
			"reason", "the verdict neither blocked the sync nor carried a warning")
		return
	}

	ref := &corev1.ObjectReference{
		APIVersion: applicationAPIVersion,
		Kind:       applicationKind,
		Namespace:  namespace,
		Name:       name,
		UID:        types.UID(uid),
	}
	r.recorder.Event(ref, eventType, reason, message)
	r.metrics.RecordEvent(reason, eventType)

	// Queued rather than written. The write itself is reported by the sink,
	// which runs on the broadcaster goroutine well after this returns.
	r.logger.Debug("queued a kubernetes event",
		"app", name,
		"namespace", namespace,
		"type", eventType,
		"reason", reason,
		"uid", uid)
}

// Shutdown drains the broadcaster so events queued by the last requests are
// written before the process exits.
func (r *Recorder) Shutdown() { r.broadcaster.Shutdown() }

// describe maps a verdict onto an Event, and reports whether one is warranted.
//
// A verdict that simply passed writes nothing. Argo CD already records the sync
// itself, and a line saying the gate agreed would add a duplicate that the
// event TTL deletes within the hour anyway. The two places that answer "was
// this checked" are decisions_total and the allow line in the log.
func describe(verdict gate.Decision) (eventType, reason, message string, ok bool) {
	switch {
	case !verdict.Allowed:
		return corev1.EventTypeWarning, ReasonBlocked, truncate(verdict.Message), true
	case len(verdict.Warnings) > 0:
		return corev1.EventTypeNormal, ReasonWarned, truncate(verdict.Message), true
	default:
		return "", "", "", false
	}
}

// truncate bounds the message and says so in a sentence.
//
// The cut ends the text mid-word, so it is followed by a statement rather than
// by an ellipsis or a dash. Everything an event carries has to read as plain
// prose: punctuation that stands in for words renders inconsistently across
// kubectl, the Argo CD UI, and whatever ships the event onward.
func truncate(message string) string {
	if len(message) <= maxMessageBytes {
		return message
	}
	const note = " The rest of this message was cut because the event was too long."
	return message[:maxMessageBytes-len(note)] + note
}

// loggingSink reports write failures through the gate's own logger.
//
// Without it a missing RBAC rule surfaces only as a klog line on stderr from
// inside client-go, which reads as though it came from somewhere else entirely.
// The error is still returned so the broadcaster keeps its own retry behaviour.
type loggingSink struct {
	inner  typedcorev1.EventInterface
	logger *slog.Logger
}

func (s *loggingSink) Create(event *corev1.Event) (*corev1.Event, error) {
	out, err := s.inner.CreateWithEventNamespaceWithContext(context.Background(), event)
	s.report("create", event, err)
	return out, err
}

func (s *loggingSink) Update(event *corev1.Event) (*corev1.Event, error) {
	out, err := s.inner.UpdateWithEventNamespaceWithContext(context.Background(), event)
	s.report("update", event, err)
	return out, err
}

func (s *loggingSink) Patch(event *corev1.Event, data []byte) (*corev1.Event, error) {
	out, err := s.inner.PatchWithEventNamespaceWithContext(context.Background(), event, data)
	s.report("patch", event, err)
	return out, err
}

// report logs how the write went, both ways.
//
// The success line is the point of the exercise. It is the only confirmation
// that the reason for a refusal actually reached the Application, as opposed to
// being queued and then quietly dropped by RBAC or by the spam filter.
func (s *loggingSink) report(verb string, event *corev1.Event, err error) {
	fields := []any{
		"verb", verb,
		"app", event.InvolvedObject.Name,
		"namespace", event.InvolvedObject.Namespace,
		"type", event.Type,
		"reason", event.Reason,
		"count", event.Count,
	}
	if err != nil {
		s.logger.Warn("could not write a promotion gate event. The verdict itself was unaffected",
			append(fields, "error", err)...)
		return
	}
	s.logger.Info("wrote a promotion gate event", fields...)
}
