// Package annotations holds the annotation key prefix every subsystem writes
// under. It is its own package because more than one subsystem annotates
// cluster objects (the throughput recommender writes on Nodes, the unused
// volume scanner on PersistentVolumeClaims and PersistentVolumes) and the keys
// are this addon's published interface: one definition keeps them from drifting
// apart, and importing a whole subsystem for a string constant would couple two
// packages that share nothing else.
package annotations

// Prefix is the prefix of every annotation key this addon writes, joined to a
// key suffix as "<Prefix>/<suffix>". A single DNS label is a valid annotation
// key prefix, so this needs no domain. Changing it orphans every annotation
// already written on every object, which is not a per-install decision.
const Prefix = "external-ebs-autoresizer"

// Key joins Prefix and a key suffix into a full annotation key.
func Key(suffix string) string { return Prefix + "/" + suffix }
