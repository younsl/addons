package pvscan

import (
	"testing"

	appsv1 "k8s.io/api/apps/v1"
	corev1 "k8s.io/api/core/v1"
	"k8s.io/apimachinery/pkg/api/resource"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
)

// metaName builds the object metadata of a volumeClaimTemplate entry, which the
// StatefulSet controller uses to prefix the claims it generates.
func metaName(name string) metav1.ObjectMeta { return metav1.ObjectMeta{Name: name} }

func TestEBSVolumeID(t *testing.T) {
	cases := map[string]struct {
		spec corev1.PersistentVolumeSpec
		want string
	}{
		"csi driver": {
			spec: corev1.PersistentVolumeSpec{PersistentVolumeSource: corev1.PersistentVolumeSource{
				CSI: &corev1.CSIPersistentVolumeSource{Driver: ebsCSIDriver, VolumeHandle: "vol-0abc"}}},
			want: "vol-0abc",
		},
		"another csi driver": {
			spec: corev1.PersistentVolumeSpec{PersistentVolumeSource: corev1.PersistentVolumeSource{
				CSI: &corev1.CSIPersistentVolumeSource{Driver: "efs.csi.aws.com", VolumeHandle: "fs-0abc"}}},
			want: "",
		},
		"in-tree with a zone prefix": {
			spec: corev1.PersistentVolumeSpec{PersistentVolumeSource: corev1.PersistentVolumeSource{
				AWSElasticBlockStore: &corev1.AWSElasticBlockStoreVolumeSource{VolumeID: "aws://ap-northeast-2a/vol-0def"}}},
			want: "vol-0def",
		},
		"in-tree bare": {
			spec: corev1.PersistentVolumeSpec{PersistentVolumeSource: corev1.PersistentVolumeSource{
				AWSElasticBlockStore: &corev1.AWSElasticBlockStoreVolumeSource{VolumeID: "vol-0ghi"}}},
			want: "vol-0ghi",
		},
		"not ebs": {
			spec: corev1.PersistentVolumeSpec{PersistentVolumeSource: corev1.PersistentVolumeSource{
				HostPath: &corev1.HostPathVolumeSource{Path: "/data"}}},
			want: "",
		},
	}
	for name, tc := range cases {
		if got := ebsVolumeID(&corev1.PersistentVolume{Spec: tc.spec}); got != tc.want {
			t.Errorf("%s: got %q, want %q", name, got, tc.want)
		}
	}
}

func TestClaimCapacityBytesPrefersTheBoundSize(t *testing.T) {
	p := &corev1.PersistentVolumeClaim{
		Spec: corev1.PersistentVolumeClaimSpec{Resources: corev1.VolumeResourceRequirements{
			Requests: corev1.ResourceList{corev1.ResourceStorage: resource.MustParse("8Gi")}}},
		Status: corev1.PersistentVolumeClaimStatus{
			Capacity: corev1.ResourceList{corev1.ResourceStorage: resource.MustParse("10Gi")}},
	}
	if got, want := claimCapacityBytes(p), int64(10*1024*1024*1024); got != want {
		t.Errorf("got %d, want the bound %d", got, want)
	}
}

func TestClaimCapacityBytesFallsBackToTheRequest(t *testing.T) {
	p := &corev1.PersistentVolumeClaim{
		Spec: corev1.PersistentVolumeClaimSpec{Resources: corev1.VolumeResourceRequirements{
			Requests: corev1.ResourceList{corev1.ResourceStorage: resource.MustParse("8Gi")}}},
	}
	if got, want := claimCapacityBytes(p), int64(8*1024*1024*1024); got != want {
		t.Errorf("got %d, want the requested %d", got, want)
	}
}

func TestClaimCapacityBytesUnset(t *testing.T) {
	if got := claimCapacityBytes(&corev1.PersistentVolumeClaim{}); got != 0 {
		t.Errorf("got %d, want 0", got)
	}
}

func TestStorageClassOf(t *testing.T) {
	empty, named := "", "gp3"
	for name, tc := range map[string]struct {
		in   *string
		want string
	}{
		"nil is the default class":   {nil, ""},
		"empty means no class":       {&empty, ""},
		"a named class is passed on": {&named, "gp3"},
	} {
		if got := storageClassOf(tc.in); got != tc.want {
			t.Errorf("%s: got %q, want %q", name, got, tc.want)
		}
	}
}

func TestNewStatefulSetDefaults(t *testing.T) {
	got := newStatefulSet(&appsv1.StatefulSet{})
	if got.Replicas != 1 {
		t.Errorf("replicas = %d, want the API default of 1", got.Replicas)
	}
	if got.OrdinalStart != 0 {
		t.Errorf("ordinal start = %d, want 0", got.OrdinalStart)
	}
}

func TestNewStatefulSetReadsSpec(t *testing.T) {
	replicas := int32(3)
	in := &appsv1.StatefulSet{Spec: appsv1.StatefulSetSpec{
		Replicas: &replicas,
		Ordinals: &appsv1.StatefulSetOrdinals{Start: 5},
		VolumeClaimTemplates: []corev1.PersistentVolumeClaim{
			{ObjectMeta: metaName("data")}, {ObjectMeta: metaName("logs")},
		},
	}}
	got := newStatefulSet(in)
	if got.Replicas != 3 || got.OrdinalStart != 5 {
		t.Errorf("got replicas=%d start=%d, want 3/5", got.Replicas, got.OrdinalStart)
	}
	if len(got.ClaimTemplates) != 2 || got.ClaimTemplates[0] != "data" || got.ClaimTemplates[1] != "logs" {
		t.Errorf("claim templates = %v, want [data logs]", got.ClaimTemplates)
	}
}

func TestAnnotationPatchEmpty(t *testing.T) {
	patch, err := annotationPatch(nil, nil)
	if err != nil {
		t.Fatalf("annotationPatch: %v", err)
	}
	if patch != nil {
		t.Errorf("nothing to write must produce no patch, got %s", patch)
	}
}

func TestAnnotationPatchRemovesWithNull(t *testing.T) {
	patch, err := annotationPatch(map[string]string{"a": "1"}, []string{"b"})
	if err != nil {
		t.Fatalf("annotationPatch: %v", err)
	}
	want := `{"metadata":{"annotations":{"a":"1","b":null}}}`
	if string(patch) != want {
		t.Errorf("got %s, want %s", patch, want)
	}
}
