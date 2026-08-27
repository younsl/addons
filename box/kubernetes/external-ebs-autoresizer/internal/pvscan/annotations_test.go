package pvscan

import (
	"maps"
	"slices"
	"testing"
	"time"
)

func TestBuildAnnotationsUnused(t *testing.T) {
	since := time.Date(2026, 8, 24, 0, 0, 0, 0, time.UTC)
	a := buildAnnotations(Finding{
		Unused:      true,
		Reason:      ReasonReleased,
		UnusedSince: since,
		Age:         50 * time.Hour,
		VolumeID:    "vol-abc",
	})
	want := map[string]string{
		key(keyUnused):       "true",
		key(keyUnusedSince):  "2026-08-24T00:00:00Z",
		key(keyUnusedReason): ReasonReleased,
		key(keyUnusedDays):   "2",
		key(keyVolumeID):     "vol-abc",
	}
	for k, v := range want {
		if a.set[k] != v {
			t.Errorf("%s = %q, want %q", k, a.set[k], v)
		}
	}
	if len(a.remove) != 0 {
		t.Errorf("a fully populated finding removes nothing, got %v", a.remove)
	}
}

func TestBuildAnnotationsWithoutVolumeIDRemovesTheKey(t *testing.T) {
	a := buildAnnotations(Finding{Unused: true, Reason: ReasonUnbound})
	if !slices.Contains(a.remove, key(keyVolumeID)) {
		t.Fatalf("an unbound claim has no volume ID, so the key must be removed: %v", a.remove)
	}
}

func TestBuildAnnotationsInUseClearsEveryKey(t *testing.T) {
	a := buildAnnotations(Finding{Reason: reasonMountedByPod})
	if len(a.set) != 0 {
		t.Fatalf("an in-use object writes nothing, got %v", a.set)
	}
	for _, suffix := range append(slices.Clone(dataKeys), keyObservedAt) {
		if !slices.Contains(a.remove, key(suffix)) {
			t.Errorf("%s must be cleared when the object comes back into use", suffix)
		}
	}
}

func TestNeedsWriteSkipsAnUnmarkedInUseObject(t *testing.T) {
	a := buildAnnotations(Finding{Reason: reasonMountedByPod})
	if a.needsWrite(map[string]string{"other": "value"}, time.Now()) {
		t.Fatal("an in-use object that was never marked must not be patched")
	}
}

func TestNeedsWriteClearsAStaleMark(t *testing.T) {
	a := buildAnnotations(Finding{Reason: reasonMountedByPod})
	existing := map[string]string{key(keyUnused): "true"}
	if !a.needsWrite(existing, time.Now()) {
		t.Fatal("a mark left by an earlier pass must be cleared")
	}
}

func TestNeedsWriteSkipsUnchangedAndFresh(t *testing.T) {
	now := time.Date(2026, 8, 27, 12, 0, 0, 0, time.UTC)
	f := Finding{Unused: true, Reason: ReasonReleased, UnusedSince: now.Add(-50 * time.Hour), Age: 50 * time.Hour}
	a := buildAnnotations(f)
	existing := map[string]string{key(keyObservedAt): now.Add(-time.Hour).Format(time.RFC3339)}
	maps.Copy(existing, a.set)
	if a.needsWrite(existing, now) {
		t.Fatal("unchanged values with a fresh observed-at need no patch")
	}
}

func TestNeedsWriteRefreshesAStaleObservedAt(t *testing.T) {
	now := time.Date(2026, 8, 27, 12, 0, 0, 0, time.UTC)
	f := Finding{Unused: true, Reason: ReasonReleased, UnusedSince: now.Add(-50 * time.Hour), Age: 50 * time.Hour}
	a := buildAnnotations(f)
	existing := map[string]string{key(keyObservedAt): now.Add(-refreshInterval).Format(time.RFC3339)}
	maps.Copy(existing, a.set)
	if !a.needsWrite(existing, now) {
		t.Fatal("an observed-at older than the refresh interval must be rewritten")
	}
}

func TestNeedsWriteOnAChangedReason(t *testing.T) {
	now := time.Now()
	a := buildAnnotations(Finding{Unused: true, Reason: ReasonReleased, UnusedSince: now, Age: 0})
	existing := map[string]string{
		key(keyUnused):       "true",
		key(keyUnusedReason): ReasonAvailable,
		key(keyObservedAt):   now.Format(time.RFC3339),
	}
	if !a.needsWrite(existing, now) {
		t.Fatal("a reason that changed must be rewritten")
	}
}
