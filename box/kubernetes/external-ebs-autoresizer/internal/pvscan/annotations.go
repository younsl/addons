package pvscan

import (
	"slices"
	"strconv"
	"time"

	"github.com/younsl/o/box/kubernetes/external-ebs-autoresizer/internal/annotations"
)

// Annotation key suffixes written on each unused PersistentVolumeClaim and
// PersistentVolume, joined to the shared prefix as "<prefix>/<suffix>". Keys are
// stable identifiers: renaming one orphans the old key on every object already
// annotated.
const (
	keyUnused       = "unused"
	keyUnusedSince  = "unused-since"
	keyUnusedReason = "unused-reason"
	keyUnusedDays   = "unused-days"
	keyVolumeID     = "volume-id"
	keyObservedAt   = "unused-observed-at"
)

// dataKeys are every key except unused-observed-at, in a fixed order. It is
// excluded because it changes on every pass and would make every comparison
// report a difference, defeating the skip-unchanged check.
var dataKeys = []string{keyUnused, keyUnusedSince, keyUnusedReason, keyUnusedDays, keyVolumeID}

// refreshInterval is how stale unused-observed-at may get before the annotations
// are rewritten even though nothing changed. Without it an object that has been
// unused for months would carry the timestamp of the day it was first marked,
// leaving an operator unable to tell a current reading from a stopped scanner.
const refreshInterval = 24 * time.Hour

// key joins the shared prefix and a key suffix.
func key(suffix string) string { return annotations.Key(suffix) }

// annotationSet is one object's desired annotations: values to write and keys to
// remove. Removal is what makes a claim that came back into use stop reading as
// unused, which matters more here than for an advisory value: a stale "unused"
// mark is an invitation to delete live data.
type annotationSet struct {
	set    map[string]string
	remove []string
}

// buildAnnotations renders one finding into annotation values. An object in use
// sets nothing and queues every key for removal.
//
// unused-days is derivable from unused-since, but only outside kubectl:
// custom-columns cannot subtract two timestamps. Writing it is what makes a
// cluster's claims sortable by how long they have been dead.
func buildAnnotations(f Finding) annotationSet {
	set := map[string]string{}
	if f.Unused {
		set[key(keyUnused)] = "true"
		set[key(keyUnusedSince)] = f.UnusedSince.UTC().Format(time.RFC3339)
		set[key(keyUnusedReason)] = f.Reason
		set[key(keyUnusedDays)] = strconv.Itoa(int(f.Age / (24 * time.Hour)))
		if f.VolumeID != "" {
			set[key(keyVolumeID)] = f.VolumeID
		}
	}
	var remove []string
	for _, suffix := range dataKeys {
		if _, ok := set[key(suffix)]; !ok {
			remove = append(remove, key(suffix))
		}
	}
	// unused-observed-at is not in dataKeys, since it changes every pass and would
	// defeat the skip-unchanged check. It still has to be cleared alongside them
	// when the object comes back into use, or the object would keep a timestamp
	// with nothing left to timestamp.
	if !f.Unused {
		remove = append(remove, key(keyObservedAt))
	}
	return annotationSet{set: set, remove: remove}
}

// needsWrite reports whether the object has to be patched: any value differs, a
// key queued for removal is still present, or unused-observed-at has gone stale
// past refreshInterval.
//
// An in-use object writes nothing, so it is patched only to clear a mark left by
// an earlier pass. Without that short-circuit every healthy claim in the cluster
// would be patched on every pass to refresh a timestamp it does not carry.
func (a annotationSet) needsWrite(existing map[string]string, now time.Time) bool {
	stillPresent := slices.ContainsFunc(a.remove, func(k string) bool {
		_, ok := existing[k]
		return ok
	})
	if len(a.set) == 0 {
		return stillPresent
	}
	for k, v := range a.set {
		if existing[k] != v {
			return true
		}
	}
	if stillPresent {
		return true
	}
	last, err := time.Parse(time.RFC3339, existing[key(keyObservedAt)])
	if err != nil {
		return true
	}
	return now.Sub(last) >= refreshInterval
}
