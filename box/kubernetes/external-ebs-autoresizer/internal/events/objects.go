package events

import (
	"fmt"

	corev1 "k8s.io/api/core/v1"
	"k8s.io/apimachinery/pkg/types"
	"k8s.io/client-go/kubernetes"
	"k8s.io/client-go/kubernetes/scheme"
	typedcorev1 "k8s.io/client-go/kubernetes/typed/core/v1"
	"k8s.io/client-go/rest"
	"k8s.io/client-go/tools/record"
)

// ObjectEmitter publishes Events against cluster objects the addon observes but
// does not own: Nodes for the throughput recommender, PersistentVolumeClaims and
// PersistentVolumes for the unused volume scanner. Each shows up where an
// operator already looks:
//
//	kubectl describe node <node>
//	kubectl describe pvc -n <namespace> <claim>
//	kubectl describe pv <volume>
//
// It is separate from Emitter because the two write to different places. Events
// about standalone EC2 instances have no cluster object to attach to and are
// recorded against the controller's own Pod in its own namespace. The objects
// here live anywhere: a Node or a PersistentVolume is cluster-scoped, so its
// Events carry no namespace of their own and the API server stores them in
// "default" (the same as the kubelet's own node Events), while a claim's Events
// belong in the claim's namespace. The sink is therefore bound to no namespace:
// client-go rejects an Event whose namespace differs from the one its sink was
// built with, and a sink built with "" routes each Event to its own namespace
// instead. Reusing the Pod-namespaced sink here would fail every write.
type ObjectEmitter struct {
	recorder    record.EventRecorder
	broadcaster record.EventBroadcaster
}

// NewObjectEmitter builds an ObjectEmitter using the in-cluster config. It fails
// outside a cluster, which the caller treats as "disable these Events" rather
// than a fatal error.
func NewObjectEmitter() (*ObjectEmitter, error) {
	cfg, err := rest.InClusterConfig()
	if err != nil {
		return nil, fmt.Errorf("in-cluster config: %w", err)
	}
	clientset, err := kubernetes.NewForConfig(cfg)
	if err != nil {
		return nil, fmt.Errorf("kubernetes client: %w", err)
	}

	broadcaster := record.NewBroadcaster()
	broadcaster.StartRecordingToSink(&typedcorev1.EventSinkImpl{
		Interface: clientset.CoreV1().Events(""),
	})
	return &ObjectEmitter{
		recorder:    broadcaster.NewRecorder(scheme.Scheme, corev1.EventSource{Component: component}),
		broadcaster: broadcaster,
	}, nil
}

// NodeEventf records an Event against one Node. uid may be empty on any of these
// methods: the recorder then resolves the object by name alone, which is enough
// for kubectl to associate the Event.
//
// Repeating the same reason for the same object does not create a new Event
// object. The recorder aggregates it into the existing one and increments its
// count, which is what makes a per-object, per-pass Event affordable on a large
// cluster.
func (e *ObjectEmitter) NodeEventf(name, uid, eventType, reason, messageFmt string, args ...any) {
	e.eventf(&corev1.ObjectReference{
		Kind:       "Node",
		APIVersion: "v1",
		Name:       name,
		UID:        types.UID(uid),
	}, eventType, reason, messageFmt, args...)
}

// ClaimEventf records an Event against one PersistentVolumeClaim. The claim is
// namespaced, so its Events are stored in its own namespace and appear in
// kubectl describe pvc without a namespace flag of their own.
func (e *ObjectEmitter) ClaimEventf(namespace, name, uid, eventType, reason, messageFmt string, args ...any) {
	e.eventf(&corev1.ObjectReference{
		Kind:       "PersistentVolumeClaim",
		APIVersion: "v1",
		Namespace:  namespace,
		Name:       name,
		UID:        types.UID(uid),
	}, eventType, reason, messageFmt, args...)
}

// VolumeEventf records an Event against one PersistentVolume. Like a Node it is
// cluster-scoped, so the API server stores its Events in "default".
func (e *ObjectEmitter) VolumeEventf(name, uid, eventType, reason, messageFmt string, args ...any) {
	e.eventf(&corev1.ObjectReference{
		Kind:       "PersistentVolume",
		APIVersion: "v1",
		Name:       name,
		UID:        types.UID(uid),
	}, eventType, reason, messageFmt, args...)
}

func (e *ObjectEmitter) eventf(ref *corev1.ObjectReference, eventType, reason, messageFmt string, args ...any) {
	e.recorder.Eventf(ref, eventType, reason, messageFmt, args...)
}

// Shutdown flushes buffered Events and stops the broadcaster.
func (e *ObjectEmitter) Shutdown() {
	e.broadcaster.Shutdown()
}
