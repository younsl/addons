package pvscan

import (
	"context"
	"testing"

	appsv1 "k8s.io/api/apps/v1"
	corev1 "k8s.io/api/core/v1"
	"k8s.io/apimachinery/pkg/api/resource"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"k8s.io/apimachinery/pkg/runtime"
	"k8s.io/client-go/kubernetes/fake"
)

// newTestClient builds a Client backed by the generated fake clientset, so the
// list and patch paths run against a real decoder rather than a hand-rolled stub.
func newTestClient(objects ...runtime.Object) *Client {
	cs := fake.NewSimpleClientset(objects...)
	return &Client{core: cs.CoreV1(), apps: cs.AppsV1()}
}

func TestInventoryReadsEveryObjectKind(t *testing.T) {
	replicas := int32(1)
	c := newTestClient(
		&corev1.PersistentVolumeClaim{
			Namespace: "web", Name: "data", UID: "uid-1",
			Spec: corev1.PersistentVolumeClaimSpec{VolumeName: "pv-1"},
			Status: corev1.PersistentVolumeClaimStatus{
				Phase:    corev1.ClaimBound,
				Capacity: corev1.ResourceList{corev1.ResourceStorage: resource.MustParse("10Gi")},
			},
		},
		&corev1.PersistentVolume{
			Name: "pv-1",
			Spec: corev1.PersistentVolumeSpec{
				Capacity:                      corev1.ResourceList{corev1.ResourceStorage: resource.MustParse("10Gi")},
				StorageClassName:              "gp3",
				PersistentVolumeReclaimPolicy: corev1.PersistentVolumeReclaimRetain,
				ClaimRef:                      &corev1.ObjectReference{Namespace: "web", Name: "data", UID: "uid-1"},
				PersistentVolumeSource: corev1.PersistentVolumeSource{
					CSI: &corev1.CSIPersistentVolumeSource{Driver: ebsCSIDriver, VolumeHandle: "vol-0abc"}},
			},
			Status: corev1.PersistentVolumeStatus{Phase: corev1.VolumeBound},
		},
		&corev1.Pod{
			Namespace: "web", Name: "app",
			Spec: corev1.PodSpec{Volumes: []corev1.Volume{{
				Name:                  "data",
				PersistentVolumeClaim: &corev1.PersistentVolumeClaimVolumeSource{ClaimName: "data"},
			}}},
			Status: corev1.PodStatus{Phase: corev1.PodRunning},
		},
		&appsv1.StatefulSet{
			Namespace: "db", Name: "pg",
			Spec: appsv1.StatefulSetSpec{
				Replicas:             &replicas,
				VolumeClaimTemplates: []corev1.PersistentVolumeClaim{{Name: "data"}},
			},
		},
	)

	inv, err := c.Inventory(context.Background())
	if err != nil {
		t.Fatalf("Inventory: %v", err)
	}
	if len(inv.PVCs) != 1 || inv.PVCs[0].Phase != "Bound" || inv.PVCs[0].CapacityBytes != 10*1024*1024*1024 {
		t.Fatalf("claims = %+v", inv.PVCs)
	}
	if len(inv.PVs) != 1 || inv.PVs[0].VolumeID != "vol-0abc" || inv.PVs[0].ReclaimPolicy != "Retain" {
		t.Fatalf("volumes = %+v", inv.PVs)
	}
	if inv.PVs[0].ClaimName != "data" || inv.PVs[0].ClaimUID != "uid-1" {
		t.Errorf("claim reference = %+v, want web/data uid-1", inv.PVs[0])
	}
	if _, ok := inv.ClaimsInUse["web/data"]; !ok {
		t.Errorf("claims in use = %v, want web/data", inv.ClaimsInUse)
	}
	if len(inv.StatefulSets) != 1 || len(inv.StatefulSets[0].ClaimTemplates) != 1 {
		t.Errorf("statefulsets = %+v", inv.StatefulSets)
	}
}

func TestInventoryIgnoresTerminalPods(t *testing.T) {
	pod := func(name string, phase corev1.PodPhase) *corev1.Pod {
		return &corev1.Pod{
			Namespace: "batch", Name: name,
			Spec: corev1.PodSpec{Volumes: []corev1.Volume{{
				PersistentVolumeClaim: &corev1.PersistentVolumeClaimVolumeSource{ClaimName: name},
			}}},
			Status: corev1.PodStatus{Phase: phase},
		}
	}
	c := newTestClient(
		pod("done", corev1.PodSucceeded),
		pod("crashed", corev1.PodFailed),
		pod("running", corev1.PodRunning),
	)
	inv, err := c.Inventory(context.Background())
	if err != nil {
		t.Fatalf("Inventory: %v", err)
	}
	if _, ok := inv.ClaimsInUse["batch/running"]; !ok {
		t.Error("a running pod holds its claim")
	}
	for _, name := range []string{"done", "crashed"} {
		if _, ok := inv.ClaimsInUse["batch/"+name]; ok {
			t.Errorf("a %s pod mounts nothing and must not hold its claim", name)
		}
	}
}

func TestAnnotatePVCWritesAndClears(t *testing.T) {
	c := newTestClient(&corev1.PersistentVolumeClaim{
		Namespace:   "web",
		Name:        "data",
		Annotations: map[string]string{key(keyUnused): "true", "keep": "me"},
	})
	ctx := context.Background()
	if err := c.AnnotatePVC(ctx, "web", "data", map[string]string{key(keyUnusedReason): ReasonNoConsumerPod}, []string{key(keyUnused)}); err != nil {
		t.Fatalf("AnnotatePVC: %v", err)
	}
	got, err := c.core.PersistentVolumeClaims("web").Get(ctx, "data", metav1.GetOptions{})
	if err != nil {
		t.Fatalf("get: %v", err)
	}
	if got.Annotations[key(keyUnusedReason)] != ReasonNoConsumerPod {
		t.Errorf("reason not written: %v", got.Annotations)
	}
	if _, ok := got.Annotations[key(keyUnused)]; ok {
		t.Errorf("removed key survived: %v", got.Annotations)
	}
	if got.Annotations["keep"] != "me" {
		t.Errorf("a merge patch must leave other writers' annotations alone: %v", got.Annotations)
	}
}

func TestAnnotatePVWritesAndClears(t *testing.T) {
	c := newTestClient(&corev1.PersistentVolume{
		Name: "pv-1", Annotations: map[string]string{key(keyUnused): "true"},
	})
	ctx := context.Background()
	if err := c.AnnotatePV(ctx, "pv-1", nil, []string{key(keyUnused)}); err != nil {
		t.Fatalf("AnnotatePV: %v", err)
	}
	got, err := c.core.PersistentVolumes().Get(ctx, "pv-1", metav1.GetOptions{})
	if err != nil {
		t.Fatalf("get: %v", err)
	}
	if _, ok := got.Annotations[key(keyUnused)]; ok {
		t.Errorf("removed key survived: %v", got.Annotations)
	}
}

func TestAnnotateNoopWritesNothing(t *testing.T) {
	c := newTestClient()
	if err := c.AnnotatePVC(context.Background(), "web", "missing", nil, nil); err != nil {
		t.Fatalf("an empty patch must not reach the API server: %v", err)
	}
	if err := c.AnnotatePV(context.Background(), "missing", nil, nil); err != nil {
		t.Fatalf("an empty patch must not reach the API server: %v", err)
	}
}
