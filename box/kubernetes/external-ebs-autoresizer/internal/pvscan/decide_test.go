package pvscan

import (
	"testing"
	"time"
)

func TestClassifyPVCMountedByLivePod(t *testing.T) {
	inv := Inventory{
		PVCs:        []PVC{{Namespace: "web", Name: "data", Phase: phaseBound}},
		ClaimsInUse: map[string]struct{}{"web/data": {}},
	}
	got := classify(inv, time.Hour, time.Now())
	if len(got) != 1 {
		t.Fatalf("want 1 finding, got %d", len(got))
	}
	if got[0].Unused {
		t.Fatalf("claim mounted by a live pod must not be unused, reason %q", got[0].Reason)
	}
	if got[0].Reason != reasonMountedByPod {
		t.Errorf("reason = %q, want %q", got[0].Reason, reasonMountedByPod)
	}
}

func TestClassifyPVCNoConsumerPod(t *testing.T) {
	inv := Inventory{PVCs: []PVC{{Namespace: "web", Name: "data", Phase: phaseBound, VolumeName: "pv-1"}},
		PVs: []PV{{Name: "pv-1", Phase: phaseBound, ClaimNamespace: "web", ClaimName: "data", VolumeID: "vol-abc"}}}
	got := classify(inv, time.Hour, time.Now())
	pvc := findingFor(t, got, KindPVC, "data")
	if !pvc.Unused || pvc.Reason != ReasonNoConsumerPod {
		t.Fatalf("got unused=%t reason=%q, want true/%s", pvc.Unused, pvc.Reason, ReasonNoConsumerPod)
	}
	if pvc.VolumeID != "vol-abc" {
		t.Errorf("volume ID = %q, want the ID of the bound volume", pvc.VolumeID)
	}
	pv := findingFor(t, got, KindPV, "pv-1")
	if !pv.Unused || pv.Reason != ReasonUnusedClaim {
		t.Fatalf("bound volume of an unused claim: got unused=%t reason=%q, want true/%s", pv.Unused, pv.Reason, ReasonUnusedClaim)
	}
}

func TestClassifyPVCUnbound(t *testing.T) {
	inv := Inventory{PVCs: []PVC{{Namespace: "web", Name: "data", Phase: "Pending"}}}
	got := classify(inv, 0, time.Now())
	if got[0].Reason != ReasonUnbound {
		t.Fatalf("reason = %q, want %q", got[0].Reason, ReasonUnbound)
	}
}

func TestClassifyStatefulSetSlots(t *testing.T) {
	sets := []StatefulSet{{Namespace: "db", Name: "pg", Replicas: 2, ClaimTemplates: []string{"data"}}}
	inv := Inventory{
		PVCs: []PVC{
			{Namespace: "db", Name: "data-pg-0", Phase: phaseBound},
			{Namespace: "db", Name: "data-pg-1", Phase: phaseBound},
			{Namespace: "db", Name: "data-pg-4", Phase: phaseBound},
		},
		StatefulSets: sets,
	}
	got := classify(inv, 0, time.Now())
	for _, tc := range []struct {
		name   string
		unused bool
		reason string
	}{
		{"data-pg-0", false, reasonStatefulSetSlot},
		{"data-pg-1", false, reasonStatefulSetSlot},
		{"data-pg-4", true, ReasonStatefulSetScaledDown},
	} {
		f := findingFor(t, got, KindPVC, tc.name)
		if f.Unused != tc.unused || f.Reason != tc.reason {
			t.Errorf("%s: got unused=%t reason=%q, want %t/%s", tc.name, f.Unused, f.Reason, tc.unused, tc.reason)
		}
	}
}

func TestClassifyStatefulSetOrdinalStart(t *testing.T) {
	inv := Inventory{
		PVCs: []PVC{
			{Namespace: "db", Name: "data-pg-0", Phase: phaseBound},
			{Namespace: "db", Name: "data-pg-5", Phase: phaseBound},
		},
		StatefulSets: []StatefulSet{{Namespace: "db", Name: "pg", Replicas: 2, OrdinalStart: 5, ClaimTemplates: []string{"data"}}},
	}
	got := classify(inv, 0, time.Now())
	if f := findingFor(t, got, KindPVC, "data-pg-0"); !f.Unused {
		t.Errorf("ordinal below the start is outside the replica range and must be unused")
	}
	if f := findingFor(t, got, KindPVC, "data-pg-5"); f.Unused {
		t.Errorf("ordinal at the start is inside the replica range and must be in use")
	}
}

func TestClassifyStatefulSetNameCollision(t *testing.T) {
	// A claim whose name starts like a template claim but does not end in an
	// ordinal is an ordinary claim, not a replica slot.
	inv := Inventory{
		PVCs:         []PVC{{Namespace: "db", Name: "data-pg-backup", Phase: phaseBound}},
		StatefulSets: []StatefulSet{{Namespace: "db", Name: "pg", Replicas: 3, ClaimTemplates: []string{"data"}}},
	}
	got := classify(inv, 0, time.Now())
	if got[0].Reason != ReasonNoConsumerPod {
		t.Fatalf("reason = %q, want %q", got[0].Reason, ReasonNoConsumerPod)
	}
}

func TestClassifyPVPhases(t *testing.T) {
	inv := Inventory{PVs: []PV{
		{Name: "pv-avail", Phase: phaseAvailable},
		{Name: "pv-released", Phase: phaseReleased, ClaimNamespace: "gone", ClaimName: "data"},
		{Name: "pv-failed", Phase: phaseFailed},
		{Name: "pv-pending", Phase: "Pending"},
		{Name: "pv-orphan", Phase: phaseBound, ClaimNamespace: "gone", ClaimName: "data"},
	}}
	got := classify(inv, 0, time.Now())
	for name, want := range map[string]string{
		"pv-avail":    ReasonAvailable,
		"pv-released": ReasonReleased,
		"pv-failed":   ReasonFailed,
		"pv-pending":  reasonPending,
		"pv-orphan":   ReasonMissingClaim,
	} {
		f := findingFor(t, got, KindPV, name)
		if f.Reason != want {
			t.Errorf("%s: reason = %q, want %q", name, f.Reason, want)
		}
	}
}

func TestClassifyPVClaimUIDMismatch(t *testing.T) {
	inv := Inventory{
		PVCs:        []PVC{{Namespace: "web", Name: "data", UID: "new", Phase: phaseBound}},
		ClaimsInUse: map[string]struct{}{"web/data": {}},
		PVs:         []PV{{Name: "pv-1", Phase: phaseBound, ClaimNamespace: "web", ClaimName: "data", ClaimUID: "old"}},
	}
	got := classify(inv, 0, time.Now())
	f := findingFor(t, got, KindPV, "pv-1")
	if !f.Unused || f.Reason != ReasonMissingClaim {
		t.Fatalf("got unused=%t reason=%q, want true/%s", f.Unused, f.Reason, ReasonMissingClaim)
	}
}

func TestFinalizeUsesPersistedClock(t *testing.T) {
	now := time.Date(2026, 8, 27, 12, 0, 0, 0, time.UTC)
	since := now.Add(-72 * time.Hour)
	inv := Inventory{PVCs: []PVC{{
		Namespace:   "web",
		Name:        "data",
		Phase:       phaseBound,
		Annotations: map[string]string{key(keyUnusedSince): since.Format(time.RFC3339)},
	}}}
	got := classify(inv, 24*time.Hour, now)
	if !got[0].Reportable {
		t.Fatalf("a claim unused for 72h must be reportable at a 24h threshold")
	}
	if !got[0].UnusedSince.Equal(since) {
		t.Errorf("unused since = %s, want the persisted %s", got[0].UnusedSince, since)
	}
	if got[0].Age != 72*time.Hour {
		t.Errorf("age = %s, want 72h", got[0].Age)
	}
}

func TestFinalizeIgnoresUnusableClock(t *testing.T) {
	now := time.Date(2026, 8, 27, 12, 0, 0, 0, time.UTC)
	for name, raw := range map[string]string{
		"unparseable":   "not-a-timestamp",
		"in the future": now.Add(time.Hour).Format(time.RFC3339),
	} {
		inv := Inventory{PVCs: []PVC{{
			Namespace:   "web",
			Name:        "data",
			Phase:       phaseBound,
			Annotations: map[string]string{key(keyUnusedSince): raw},
		}}}
		got := classify(inv, 24*time.Hour, now)
		if !got[0].UnusedSince.Equal(now) {
			t.Errorf("%s: unused since = %s, want the clock to restart at now", name, got[0].UnusedSince)
		}
		if got[0].Reportable {
			t.Errorf("%s: a restarted clock must not be reportable yet", name)
		}
	}
}

func TestFinalizeLeavesUsedObjectsWithoutAClock(t *testing.T) {
	inv := Inventory{
		PVCs:        []PVC{{Namespace: "web", Name: "data", Phase: phaseBound}},
		ClaimsInUse: map[string]struct{}{"web/data": {}},
	}
	got := classify(inv, 0, time.Now())
	if !got[0].UnusedSince.IsZero() || got[0].Reportable {
		t.Fatalf("an in-use object carries no clock, got since=%s reportable=%t", got[0].UnusedSince, got[0].Reportable)
	}
}

// findingFor returns the finding for one object, failing the test when it is
// missing.
func findingFor(t *testing.T, findings []Finding, kind, name string) Finding {
	t.Helper()
	for _, f := range findings {
		if f.Kind == kind && f.Name == name {
			return f
		}
	}
	t.Fatalf("no %s finding for %q in %+v", kind, name, findings)
	return Finding{}
}
