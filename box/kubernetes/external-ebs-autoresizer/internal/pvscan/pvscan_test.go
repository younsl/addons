package pvscan

import (
	"context"
	"errors"
	"log/slog"
	"slices"
	"testing"
	"time"
)

// fakeKube serves a fixed inventory and records every annotation patch.
type fakeKube struct {
	inv     Inventory
	err     error
	patches []patch
}

type patch struct {
	kind      string
	namespace string
	name      string
	set       map[string]string
	remove    []string
}

func (f *fakeKube) Inventory(context.Context) (Inventory, error) { return f.inv, f.err }

func (f *fakeKube) AnnotatePVC(_ context.Context, namespace, name string, set map[string]string, remove []string) error {
	f.patches = append(f.patches, patch{KindPVC, namespace, name, set, remove})
	return nil
}

func (f *fakeKube) AnnotatePV(_ context.Context, name string, set map[string]string, remove []string) error {
	f.patches = append(f.patches, patch{KindPV, "", name, set, remove})
	return nil
}

// fakeRecorder captures the observations one pass publishes.
type fakeRecorder struct {
	resets int
	pvcs   []string
	pvs    []string
	// pvcLabels and pvLabels hold the full descriptive label set of each reported
	// object, in the order the _info series carries them.
	pvcLabels [][]string
	pvLabels  [][]string
	summary   map[string]int
	bytes     map[string]int64
	errors    []string
}

func newFakeRecorder() *fakeRecorder {
	return &fakeRecorder{summary: map[string]int{}, bytes: map[string]int64{}}
}

func (r *fakeRecorder) ResetUnusedVolumes() { r.resets++ }

func (r *fakeRecorder) ObserveUnusedPVC(namespace, name, volumeName, volumeID, storageClass, reason string, _, _ float64) {
	r.pvcs = append(r.pvcs, namespace+"/"+name)
	r.pvcLabels = append(r.pvcLabels, []string{namespace, name, volumeName, volumeID, storageClass, reason})
}

func (r *fakeRecorder) ObserveUnusedPV(name, volumeID, storageClass, reason, reclaimPolicy, claimNamespace, claimName string, _, _ float64) {
	r.pvs = append(r.pvs, name)
	r.pvLabels = append(r.pvLabels, []string{name, volumeID, storageClass, reason, reclaimPolicy, claimNamespace, claimName})
}

func (r *fakeRecorder) ObserveUnusedSummary(kind, reason string, count int, capacityBytes int64) {
	r.summary[kind+"/"+reason] = count
	r.bytes[kind+"/"+reason] = capacityBytes
}

func (r *fakeRecorder) ObserveError(stage string) { r.errors = append(r.errors, stage) }

// fakeEvents records every Event the scanner publishes as
// "kind|namespace/name|reason".
type fakeEvents struct{ got []string }

func (e *fakeEvents) ClaimEventf(namespace, name, _, _, reason, _ string, _ ...any) {
	e.got = append(e.got, KindPVC+"|"+namespace+"/"+name+"|"+reason)
}

func (e *fakeEvents) VolumeEventf(name, _, _, reason, _ string, _ ...any) {
	e.got = append(e.got, KindPV+"|"+name+"|"+reason)
}

func newTestScanner(kube KubeAPI, rec Recorder, minUnusedAge time.Duration, now time.Time) *Scanner {
	return newTestScannerWithEvents(kube, rec, nil, minUnusedAge, now)
}

func newTestScannerWithEvents(kube KubeAPI, rec Recorder, ev EventEmitter, minUnusedAge time.Duration, now time.Time) *Scanner {
	return newTestScannerDry(kube, rec, ev, minUnusedAge, now, false)
}

func newTestScannerDry(kube KubeAPI, rec Recorder, ev EventEmitter, minUnusedAge time.Duration, now time.Time, dryRun bool) *Scanner {
	s := New(dryRun, kube, rec, ev, slog.New(slog.DiscardHandler))
	s.minUnusedAge = minUnusedAge
	s.now = func() time.Time { return now }
	return s
}

func TestReconcileReportsOnlyPastTheThreshold(t *testing.T) {
	now := time.Date(2026, 8, 27, 12, 0, 0, 0, time.UTC)
	old := now.Add(-72 * time.Hour).Format(time.RFC3339)
	kube := &fakeKube{inv: Inventory{PVCs: []PVC{
		{Namespace: "web", Name: "old", Phase: phaseBound, CapacityBytes: 100,
			Annotations: map[string]string{key(keyUnusedSince): old}},
		{Namespace: "web", Name: "fresh", Phase: phaseBound, CapacityBytes: 200},
	}}}
	rec := newFakeRecorder()
	s := newTestScanner(kube, rec, 24*time.Hour, now)

	n, err := s.Reconcile(context.Background())
	if err != nil {
		t.Fatalf("Reconcile: %v", err)
	}
	if n != 2 {
		t.Errorf("scanned = %d, want 2", n)
	}
	if len(rec.pvcs) != 1 || rec.pvcs[0] != "web/old" {
		t.Errorf("reported = %v, want only web/old", rec.pvcs)
	}
	if got := rec.summary[KindPVC+"/"+ReasonNoConsumerPod]; got != 1 {
		t.Errorf("summary count = %d, want 1", got)
	}
	if got := rec.bytes[KindPVC+"/"+ReasonNoConsumerPod]; got != 100 {
		t.Errorf("summary bytes = %d, want only the reported claim's 100", got)
	}
	if rec.resets != 1 {
		t.Errorf("resets = %d, want exactly one per pass", rec.resets)
	}
}

func TestReconcilePublishesEveryReasonIncludingZero(t *testing.T) {
	rec := newFakeRecorder()
	s := newTestScanner(&fakeKube{}, rec, 0, time.Now())
	if _, err := s.Reconcile(context.Background()); err != nil {
		t.Fatalf("Reconcile: %v", err)
	}
	for _, reason := range UnusedPVCReasons {
		if _, ok := rec.summary[KindPVC+"/"+reason]; !ok {
			t.Errorf("no summary series for %s, a reason that matched nothing must still read as zero", reason)
		}
	}
	for _, reason := range UnusedPVReasons {
		if _, ok := rec.summary[KindPV+"/"+reason]; !ok {
			t.Errorf("no summary series for %s", reason)
		}
	}
}

func TestReconcileAnnotatesBelowTheThreshold(t *testing.T) {
	// The clock lives in the annotation, so it has to be written on the first
	// pass that sees the object unused, not on the first pass that reports it.
	now := time.Now()
	kube := &fakeKube{inv: Inventory{PVCs: []PVC{{Namespace: "web", Name: "data", Phase: phaseBound}}}}
	s := newTestScanner(kube, newFakeRecorder(), 24*time.Hour, now)
	if _, err := s.Reconcile(context.Background()); err != nil {
		t.Fatalf("Reconcile: %v", err)
	}
	if len(kube.patches) != 1 {
		t.Fatalf("want one patch, got %d", len(kube.patches))
	}
	if kube.patches[0].set[key(keyUnusedSince)] == "" {
		t.Error("the first pass must persist the unused-since clock")
	}
	if kube.patches[0].set[key(keyObservedAt)] == "" {
		t.Error("every write stamps unused-observed-at")
	}
}

func TestReconcileSkipsPatchingHealthyObjects(t *testing.T) {
	kube := &fakeKube{inv: Inventory{
		PVCs:        []PVC{{Namespace: "web", Name: "data", Phase: phaseBound}},
		ClaimsInUse: map[string]struct{}{"web/data": {}},
	}}
	s := newTestScanner(kube, newFakeRecorder(), 0, time.Now())
	if _, err := s.Reconcile(context.Background()); err != nil {
		t.Fatalf("Reconcile: %v", err)
	}
	if len(kube.patches) != 0 {
		t.Fatalf("an unmarked in-use claim must not be patched, got %+v", kube.patches)
	}
}

func TestReconcileDryRunWritesNothing(t *testing.T) {
	kube := &fakeKube{inv: Inventory{PVCs: []PVC{{Namespace: "web", Name: "data", Phase: phaseBound}}}}
	rec := newFakeRecorder()
	s := newTestScannerDry(kube, rec, nil, 0, time.Now(), true)
	if _, err := s.Reconcile(context.Background()); err != nil {
		t.Fatalf("Reconcile: %v", err)
	}
	if len(kube.patches) != 0 {
		t.Fatalf("dry run must not patch, got %+v", kube.patches)
	}
	if len(rec.pvcs) != 1 {
		t.Errorf("dry run still reports, got %v", rec.pvcs)
	}
}

func TestReconcileInventoryFailure(t *testing.T) {
	rec := newFakeRecorder()
	s := newTestScanner(&fakeKube{err: errors.New("boom")}, rec, 0, time.Now())
	if _, err := s.Reconcile(context.Background()); err == nil {
		t.Fatal("want an error when the inventory cannot be read")
	}
	if len(rec.errors) != 1 || rec.errors[0] != "pv_inventory" {
		t.Errorf("errors = %v, want one pv_inventory", rec.errors)
	}
}

func TestFindingsDoesNotWrite(t *testing.T) {
	kube := &fakeKube{inv: Inventory{PVCs: []PVC{{Namespace: "web", Name: "data", Phase: phaseBound}}}}
	s := newTestScanner(kube, newFakeRecorder(), 0, time.Now())
	got, err := s.Findings(context.Background())
	if err != nil {
		t.Fatalf("Findings: %v", err)
	}
	if len(got) != 1 || !got[0].Unused {
		t.Fatalf("got %+v, want the unused claim", got)
	}
	if len(kube.patches) != 0 {
		t.Fatalf("Findings must never write, got %+v", kube.patches)
	}
}

func TestReconcileClearsAMarkWhenAClaimComesBackIntoUse(t *testing.T) {
	kube := &fakeKube{inv: Inventory{
		PVCs: []PVC{{Namespace: "web", Name: "data", Phase: phaseBound,
			Annotations: map[string]string{key(keyUnused): "true", key(keyUnusedReason): ReasonNoConsumerPod}}},
		ClaimsInUse: map[string]struct{}{"web/data": {}},
	}}
	s := newTestScanner(kube, newFakeRecorder(), 0, time.Now())
	if _, err := s.Reconcile(context.Background()); err != nil {
		t.Fatalf("Reconcile: %v", err)
	}
	if len(kube.patches) != 1 {
		t.Fatalf("want one clearing patch, got %d", len(kube.patches))
	}
	if len(kube.patches[0].set) != 0 {
		t.Errorf("a clearing patch writes nothing, got %v", kube.patches[0].set)
	}
	if len(kube.patches[0].remove) == 0 {
		t.Error("a clearing patch removes the keys the earlier pass wrote")
	}
}

func TestReconcileReportsTheFullLabelSet(t *testing.T) {
	now := time.Date(2026, 8, 27, 12, 0, 0, 0, time.UTC)
	old := now.Add(-72 * time.Hour).Format(time.RFC3339)
	stale := map[string]string{key(keyUnusedSince): old}
	kube := &fakeKube{inv: Inventory{
		PVCs: []PVC{{Namespace: "legacy", Name: "uploads", UID: "uid-1", Phase: phaseBound,
			VolumeName: "pv-1", StorageClass: "gp3", CapacityBytes: 20, Annotations: stale}},
		PVs: []PV{
			{Name: "pv-1", Phase: phaseBound, StorageClass: "gp3", ReclaimPolicy: "Delete",
				ClaimNamespace: "legacy", ClaimName: "uploads", ClaimUID: "uid-1",
				VolumeID: "vol-0abc", CapacityBytes: 20, Annotations: stale},
			{Name: "pv-2", Phase: phaseReleased, StorageClass: "gp3", ReclaimPolicy: "Retain",
				ClaimNamespace: "gone", ClaimName: "reports",
				VolumeID: "vol-0def", CapacityBytes: 100, Annotations: stale},
		},
	}}
	rec := newFakeRecorder()
	s := newTestScanner(kube, rec, 24*time.Hour, now)
	if _, err := s.Reconcile(context.Background()); err != nil {
		t.Fatalf("Reconcile: %v", err)
	}

	wantPVC := []string{"legacy", "uploads", "pv-1", "vol-0abc", "gp3", ReasonNoConsumerPod}
	if len(rec.pvcLabels) != 1 || !slices.Equal(rec.pvcLabels[0], wantPVC) {
		t.Errorf("claim labels = %v, want %v", rec.pvcLabels, wantPVC)
	}
	wantPV := [][]string{
		{"pv-1", "vol-0abc", "gp3", ReasonUnusedClaim, "Delete", "legacy", "uploads"},
		{"pv-2", "vol-0def", "gp3", ReasonReleased, "Retain", "gone", "reports"},
	}
	if len(rec.pvLabels) != 2 {
		t.Fatalf("volume labels = %v, want 2 rows", rec.pvLabels)
	}
	for i, want := range wantPV {
		if !slices.Equal(rec.pvLabels[i], want) {
			t.Errorf("volume labels[%d] = %v, want %v", i, rec.pvLabels[i], want)
		}
	}
}

func TestBoundTo(t *testing.T) {
	for name, tc := range map[string]struct {
		f    Finding
		want string
	}{
		"claim names its volume":      {Finding{Kind: KindPVC, VolumeName: "pv-1"}, "pv-1"},
		"unbound claim names none":    {Finding{Kind: KindPVC}, ""},
		"volume names its claim":      {Finding{Kind: KindPV, ClaimNamespace: "web", ClaimName: "data"}, "web/data"},
		"unclaimed volume names none": {Finding{Kind: KindPV}, ""},
	} {
		if got := tc.f.BoundTo(); got != tc.want {
			t.Errorf("%s: got %q, want %q", name, got, tc.want)
		}
	}
}

func TestEmitDetectedOnlyPastTheThreshold(t *testing.T) {
	now := time.Date(2026, 8, 27, 12, 0, 0, 0, time.UTC)
	old := now.Add(-72 * time.Hour).Format(time.RFC3339)
	kube := &fakeKube{inv: Inventory{
		PVCs: []PVC{
			{Namespace: "legacy", Name: "old", Phase: phaseBound,
				Annotations: map[string]string{key(keyUnusedSince): old}},
			{Namespace: "legacy", Name: "fresh", Phase: phaseBound},
		},
		PVs: []PV{{Name: "pv-2", Phase: phaseReleased,
			Annotations: map[string]string{key(keyUnusedSince): old}}},
	}}
	ev := &fakeEvents{}
	s := newTestScannerWithEvents(kube, newFakeRecorder(), ev, 24*time.Hour, now)
	if _, err := s.Reconcile(context.Background()); err != nil {
		t.Fatalf("Reconcile: %v", err)
	}
	want := []string{
		KindPVC + "|legacy/old|" + reasonUnusedDetected,
		KindPV + "|pv-2|" + reasonUnusedDetected,
	}
	if !slices.Equal(ev.got, want) {
		t.Errorf("events = %v, want %v", ev.got, want)
	}
}

func TestEmitClearedOnlyWhenAMarkWasErased(t *testing.T) {
	kube := &fakeKube{inv: Inventory{
		PVCs: []PVC{
			{Namespace: "web", Name: "marked", Phase: phaseBound,
				Annotations: map[string]string{key(keyUnused): "true"}},
			{Namespace: "web", Name: "clean", Phase: phaseBound},
		},
		ClaimsInUse: map[string]struct{}{"web/marked": {}, "web/clean": {}},
	}}
	ev := &fakeEvents{}
	s := newTestScannerWithEvents(kube, newFakeRecorder(), ev, 0, time.Now())
	if _, err := s.Reconcile(context.Background()); err != nil {
		t.Fatalf("Reconcile: %v", err)
	}
	want := []string{KindPVC + "|web/marked|" + reasonUnusedCleared}
	if !slices.Equal(ev.got, want) {
		t.Errorf("events = %v, want %v: a claim that never carried a mark has no transition to report", ev.got, want)
	}
}

func TestEmitNothingOnDryRun(t *testing.T) {
	now := time.Now()
	kube := &fakeKube{inv: Inventory{PVCs: []PVC{{Namespace: "web", Name: "data", Phase: phaseBound}}}}
	ev := &fakeEvents{}
	s := newTestScannerDry(kube, newFakeRecorder(), ev, 0, now, true)
	if _, err := s.Reconcile(context.Background()); err != nil {
		t.Fatalf("Reconcile: %v", err)
	}
	if len(ev.got) != 0 {
		t.Fatalf("a dry run writes nothing anywhere, got %v", ev.got)
	}
}

func TestReconcileWithoutAnEmitter(t *testing.T) {
	kube := &fakeKube{inv: Inventory{PVCs: []PVC{{Namespace: "web", Name: "data", Phase: phaseBound}}}}
	s := newTestScanner(kube, newFakeRecorder(), 0, time.Now())
	if _, err := s.Reconcile(context.Background()); err != nil {
		t.Fatalf("a nil emitter disables Events rather than failing the pass: %v", err)
	}
}

func TestDescribeCapacity(t *testing.T) {
	for name, tc := range map[string]struct {
		f    Finding
		want string
	}{
		"size and volume": {Finding{CapacityBytes: 20 * 1024 * 1024 * 1024, VolumeID: "vol-0abc"}, "20Gi on vol-0abc"},
		"size only":       {Finding{CapacityBytes: 20 * 1024 * 1024 * 1024}, "20Gi"},
		"never bound":     {Finding{}, "no provisioned capacity"},
	} {
		if got := describeCapacity(tc.f); got != tc.want {
			t.Errorf("%s: got %q, want %q", name, got, tc.want)
		}
	}
}
