package pvscan

import (
	"strconv"
	"strings"
	"time"
)

// classify turns one pass's inventory into a Finding per PersistentVolumeClaim
// and PersistentVolume.
//
// Claims are classified first because a volume's verdict can depend on its
// claim's: a bound volume is only unused if the claim holding it is, and
// deciding that twice from different data would let the two disagree.
//
// Every namespace is in scope. A namespace filter would be the one setting that
// changes which objects the report covers rather than how each one is judged,
// and a report an operator has to remember they narrowed is worse than a longer
// one they can filter in the query.
func classify(inv Inventory, minAge time.Duration, now time.Time) []Finding {
	// The claim-to-volume lookup is built once: resolving it per claim would make
	// the pass quadratic on a cluster with thousands of volumes.
	volumeID := make(map[string]string, len(inv.PVs))
	for _, v := range inv.PVs {
		volumeID[v.Name] = v.VolumeID
	}

	// StatefulSets are indexed by namespace for the same reason: the claim loop
	// would otherwise rescan every StatefulSet in the cluster per claim.
	setsByNamespace := make(map[string][]StatefulSet, len(inv.StatefulSets))
	for _, s := range inv.StatefulSets {
		setsByNamespace[s.Namespace] = append(setsByNamespace[s.Namespace], s)
	}

	claimUnused := make(map[string]bool, len(inv.PVCs))
	claimUID := make(map[string]string, len(inv.PVCs))

	out := make([]Finding, 0, len(inv.PVCs)+len(inv.PVs))
	for _, c := range inv.PVCs {
		key := c.Namespace + "/" + c.Name
		unused, reason := classifyPVC(c, inv.ClaimsInUse, setsByNamespace[c.Namespace], key)
		claimUnused[key] = unused
		claimUID[key] = c.UID
		out = append(out, finalize(Finding{
			Kind:          KindPVC,
			Namespace:     c.Namespace,
			Name:          c.Name,
			UID:           c.UID,
			Unused:        unused,
			Reason:        reason,
			CapacityBytes: c.CapacityBytes,
			StorageClass:  c.StorageClass,
			VolumeID:      volumeID[c.VolumeName],
			VolumeName:    c.VolumeName,
			Annotations:   c.Annotations,
		}, minAge, now))
	}

	for _, v := range inv.PVs {
		key := v.ClaimNamespace + "/" + v.ClaimName
		unused, reason := classifyPV(v, key, claimUnused, claimUID)
		out = append(out, finalize(Finding{
			Kind:           KindPV,
			Name:           v.Name,
			UID:            v.UID,
			Unused:         unused,
			Reason:         reason,
			CapacityBytes:  v.CapacityBytes,
			StorageClass:   v.StorageClass,
			VolumeID:       v.VolumeID,
			ClaimNamespace: v.ClaimNamespace,
			ClaimName:      v.ClaimName,
			ReclaimPolicy:  v.ReclaimPolicy,
			Annotations:    v.Annotations,
		}, minAge, now))
	}
	return out
}

// classifyPVC decides whether one claim is unused and why.
//
// A Pod in a terminal phase is not a consumer: a completed Job's Pod object can
// outlive its run by hours and mounts nothing, so counting it would hide exactly
// the claim this scanner exists to surface. That filter is applied where the Pod
// list is built.
func classifyPVC(c PVC, inUse map[string]struct{}, sets []StatefulSet, key string) (bool, string) {
	if _, ok := inUse[key]; ok {
		return false, reasonMountedByPod
	}
	// A volumeClaimTemplate claim within its StatefulSet's replica range has no
	// Pod between a delete and the next schedule, and during a rolling update
	// every replica passes through that gap. Treating the gap as unused would
	// report the whole StatefulSet on any pass that lands mid-update.
	if live, matched := statefulSetSlot(c, sets); matched {
		if live {
			return false, reasonStatefulSetSlot
		}
		return true, ReasonStatefulSetScaledDown
	}
	if c.Phase != phaseBound {
		return true, ReasonUnbound
	}
	return true, ReasonNoConsumerPod
}

// classifyPV decides whether one volume is unused and why.
func classifyPV(v PV, key string, claimUnused map[string]bool, claimUID map[string]string) (bool, string) {
	switch v.Phase {
	case phaseAvailable:
		return true, ReasonAvailable
	case phaseReleased:
		return true, ReasonReleased
	case phaseFailed:
		return true, ReasonFailed
	case phaseBound:
		if v.ClaimName == "" {
			return true, ReasonMissingClaim
		}
		unused, known := claimUnused[key]
		if !known {
			return true, ReasonMissingClaim
		}
		// An empty claimRef UID means the volume was pre-bound by hand rather than
		// by the binder, so there is no recorded identity to compare against and a
		// name match is all the evidence there is.
		if v.ClaimUID != "" && claimUID[key] != v.ClaimUID {
			return true, ReasonMissingClaim
		}
		if unused {
			return true, ReasonUnusedClaim
		}
		return false, reasonBoundToUsedClaim
	default:
		// Pending, or a phase this build does not know. Provisioning is in flight;
		// calling it unused would report every volume being created.
		return false, reasonPending
	}
}

// statefulSetSlot reports whether a claim was generated from a
// volumeClaimTemplate and, if so, whether its ordinal is inside the
// StatefulSet's current replica range.
//
// The match is by name because the StatefulSet controller sets no owner
// reference on the claims it generates: deleting a StatefulSet deliberately
// leaves its claims behind, which garbage collection would undo.
func statefulSetSlot(c PVC, sets []StatefulSet) (live, matched bool) {
	for _, s := range sets {
		if s.Namespace != c.Namespace {
			continue
		}
		for _, tmpl := range s.ClaimTemplates {
			rest, ok := strings.CutPrefix(c.Name, tmpl+"-"+s.Name+"-")
			if !ok {
				continue
			}
			ordinal, err := strconv.ParseInt(rest, 10, 32)
			if err != nil {
				continue
			}
			n := int32(ordinal)
			return n >= s.OrdinalStart && n < s.OrdinalStart+s.Replicas, true
		}
	}
	return false, false
}

// finalize resolves how long the object has been unused and whether that is long
// enough to report. The clock is read from the annotation the previous pass
// wrote, so it survives a restart of the controller; an object that has never
// been marked starts its clock now.
func finalize(f Finding, minAge time.Duration, now time.Time) Finding {
	if !f.Unused {
		return f
	}
	f.UnusedSince = unusedSince(f.Annotations, now)
	f.Age = now.Sub(f.UnusedSince)
	f.Reportable = f.Age >= minAge
	return f
}

// unusedSince reads the persisted first-observed timestamp, falling back to now
// for an object that carries none or carries one that does not parse. A
// timestamp in the future is also discarded: a clock that ran backwards would
// otherwise hold the object below the threshold indefinitely.
func unusedSince(existing map[string]string, now time.Time) time.Time {
	t, err := time.Parse(time.RFC3339, existing[key(keyUnusedSince)])
	if err != nil || t.After(now) {
		return now
	}
	return t
}
