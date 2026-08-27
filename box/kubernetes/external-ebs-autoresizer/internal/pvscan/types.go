package pvscan

import "time"

// Kinds reported by the scanner. They are the lowercase Kubernetes resource
// names rather than the Go type names, so a metric label or a log field matches
// what an operator types into kubectl.
const (
	KindPVC = "persistentvolumeclaim"
	KindPV  = "persistentvolume"
)

// Reasons a PersistentVolumeClaim is unused.
const (
	// ReasonNoConsumerPod is a bound claim that no live Pod mounts. This is the
	// ordinary leak: the workload that owned the claim is gone, the claim is not,
	// and the EBS volume behind it keeps billing.
	ReasonNoConsumerPod = "no_consumer_pod"
	// ReasonStatefulSetScaledDown is a volumeClaimTemplate claim whose ordinal is
	// outside its StatefulSet's current replica range. Scaling a StatefulSet down
	// deliberately leaves these behind so scaling back up reattaches the same
	// data, which makes them the leak that survives longest unnoticed.
	ReasonStatefulSetScaledDown = "statefulset_scaled_down"
	// ReasonUnbound is a claim that never bound to a volume and has no Pod to
	// trigger binding. It costs nothing yet, and it never will: nothing is coming
	// to consume it.
	ReasonUnbound = "unbound"
)

// Reasons a PersistentVolume is unused.
const (
	// ReasonAvailable is a volume that has never been claimed.
	ReasonAvailable = "available"
	// ReasonReleased is a volume whose claim was deleted under a Retain reclaim
	// policy. Kubernetes will never reuse it and never delete it, so the EBS
	// volume behind it outlives every trace of the workload.
	ReasonReleased = "released"
	// ReasonFailed is a volume whose automatic reclamation failed.
	ReasonFailed = "failed"
	// ReasonMissingClaim is a volume that still claims to be Bound while the claim
	// it names does not exist, or exists with a different UID (deleted and
	// recreated under the same name).
	ReasonMissingClaim = "missing_claim"
	// ReasonUnusedClaim is a volume bound to a claim that is itself unused. It is
	// reported separately from the claim so a report by volume is complete on its
	// own, and so the capacity is counted where it is actually provisioned.
	ReasonUnusedClaim = "bound_to_unused_claim"
)

// Reasons an object is in use, recorded so a per-object log line says why the
// scanner left it alone. They are never reported as findings.
const (
	reasonMountedByPod     = "mounted_by_pod"
	reasonStatefulSetSlot  = "statefulset_slot"
	reasonBoundToUsedClaim = "bound_to_used_claim"
	reasonPending          = "pending"
)

// UnusedPVCReasons and UnusedPVReasons are the fixed reason sets, in report
// order. The summary gauge is published for every entry on every pass, including
// the ones that matched nothing, so a query for a reason that has stopped
// occurring reads as zero rather than as no data.
var (
	UnusedPVCReasons = []string{ReasonNoConsumerPod, ReasonStatefulSetScaledDown, ReasonUnbound}
	UnusedPVReasons  = []string{ReasonReleased, ReasonAvailable, ReasonFailed, ReasonMissingClaim, ReasonUnusedClaim}
)

// Kubernetes phase values the classifier switches on.
const (
	phaseBound     = "Bound"
	phaseAvailable = "Available"
	phaseReleased  = "Released"
	phaseFailed    = "Failed"
)

// PVC is the subset of a PersistentVolumeClaim the scanner needs.
type PVC struct {
	Namespace string
	Name      string
	// UID distinguishes this claim from a deleted one that had the same name, so
	// a PersistentVolume still pointing at the old UID reads as orphaned rather
	// than as bound to the new claim.
	UID           string
	Phase         string
	VolumeName    string
	StorageClass  string
	CapacityBytes int64
	Annotations   map[string]string
}

// PV is the subset of a PersistentVolume the scanner needs.
type PV struct {
	Name string
	// UID identifies the live object, so an Event recorded against it is
	// associated with this volume rather than a recycled name.
	UID           string
	Phase         string
	StorageClass  string
	CapacityBytes int64
	ReclaimPolicy string
	// ClaimNamespace, ClaimName, and ClaimUID come from spec.claimRef. The UID is
	// empty on a volume that was pre-bound by an operator rather than by the
	// binder, which is why a mismatch is only checked when it is set.
	ClaimNamespace string
	ClaimName      string
	ClaimUID       string
	// VolumeID is the EBS volume ID behind the volume, empty when it is not
	// EBS-backed. It is what turns a finding into something an operator can price
	// and delete in EC2.
	VolumeID    string
	Annotations map[string]string
}

// StatefulSet is the subset of a StatefulSet the scanner needs to tell a claim
// held open for a live replica slot from one left behind by a scale-down.
type StatefulSet struct {
	Namespace string
	Name      string
	Replicas  int32
	// OrdinalStart is spec.ordinals.start, 0 unless the StatefulSet sets it.
	OrdinalStart int32
	// ClaimTemplates are the volumeClaimTemplate names, which prefix the claim
	// names the controller generates.
	ClaimTemplates []string
}

// Inventory is one pass's cluster snapshot.
type Inventory struct {
	PVCs []PVC
	PVs  []PV
	// ClaimsInUse holds "namespace/name" for every claim a live Pod references.
	ClaimsInUse  map[string]struct{}
	StatefulSets []StatefulSet
}

// Finding is one object's verdict for one pass.
type Finding struct {
	Kind string
	// Namespace is empty for a PersistentVolume, which is cluster-scoped.
	Namespace string
	Name      string
	// UID identifies the live object for an Event recorded against it.
	UID    string
	Unused bool
	Reason string
	// UnusedSince is when the object was first observed unused, carried across
	// restarts by the annotation the scanner writes. Zero when the object is in
	// use.
	UnusedSince time.Time
	// Age is how long the object has been unused, and Reportable is whether that
	// has passed minUnusedAge. An unused object below the threshold is still
	// annotated (that is where the clock lives) but is not yet reported.
	Age        time.Duration
	Reportable bool

	CapacityBytes int64
	StorageClass  string
	VolumeID      string
	// VolumeName is the PersistentVolume a claim is bound to, empty for a claim
	// that never bound and on a volume finding.
	VolumeName string
	// ClaimNamespace and ClaimName are the claim a volume is (or was) bound to,
	// empty for a volume that was never claimed and on a claim finding. They are
	// what lets a report listed by volume still name the workload that left it
	// behind, which is the whole reason a released volume is worth looking at.
	ClaimNamespace string
	ClaimName      string
	ReclaimPolicy  string
	// Annotations is the object's current annotation set, used to skip a patch
	// when nothing changed.
	Annotations map[string]string
}

// Key identifies the finding's object for logs and errors.
func (f Finding) Key() string {
	if f.Namespace == "" {
		return f.Name
	}
	return f.Namespace + "/" + f.Name
}

// BoundTo names the object on the other side of the binding: the volume for a
// claim, the claim for a volume. Empty when nothing is bound.
func (f Finding) BoundTo() string {
	if f.Kind == KindPVC {
		return f.VolumeName
	}
	if f.ClaimName == "" {
		return ""
	}
	return f.ClaimNamespace + "/" + f.ClaimName
}
