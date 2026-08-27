package pvscan

import (
	"context"
	"encoding/json"
	"fmt"
	"strings"

	appsv1 "k8s.io/api/apps/v1"
	corev1 "k8s.io/api/core/v1"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"k8s.io/apimachinery/pkg/types"
	"k8s.io/client-go/kubernetes"
	typedappsv1 "k8s.io/client-go/kubernetes/typed/apps/v1"
	typedcorev1 "k8s.io/client-go/kubernetes/typed/core/v1"
	"k8s.io/client-go/rest"
)

// ebsCSIDriver is the CSI driver name of the AWS EBS CSI driver. A volume
// provisioned by it carries the EBS volume ID verbatim in
// spec.csi.volumeHandle.
const ebsCSIDriver = "ebs.csi.aws.com"

// pageSize bounds each list request, so a cluster with tens of thousands of
// claims never issues one unbounded request.
const pageSize = 500

// Client reads the cluster objects the scanner classifies and writes its verdict
// back as annotations. Like the Node client, it is deliberately limited to list
// and annotate: nothing here can delete a claim or a volume.
type Client struct {
	core typedcorev1.CoreV1Interface
	apps typedappsv1.AppsV1Interface
}

// NewClient builds a Client from the in-cluster config. It fails outside a
// cluster, which the caller treats as "disable the scanner" rather than a fatal
// error.
func NewClient() (*Client, error) {
	cfg, err := rest.InClusterConfig()
	if err != nil {
		return nil, fmt.Errorf("in-cluster config: %w", err)
	}
	clientset, err := kubernetes.NewForConfig(cfg)
	if err != nil {
		return nil, fmt.Errorf("kubernetes client: %w", err)
	}
	return &Client{core: clientset.CoreV1(), apps: clientset.AppsV1()}, nil
}

// Inventory reads the whole cluster's claims, volumes, Pod claim references, and
// StatefulSets in four paginated list sweeps.
func (c *Client) Inventory(ctx context.Context) (Inventory, error) {
	pvcs, err := c.listPVCs(ctx)
	if err != nil {
		return Inventory{}, err
	}
	pvs, err := c.listPVs(ctx)
	if err != nil {
		return Inventory{}, err
	}
	inUse, err := c.listClaimsInUse(ctx)
	if err != nil {
		return Inventory{}, err
	}
	sets, err := c.listStatefulSets(ctx)
	if err != nil {
		return Inventory{}, err
	}
	return Inventory{PVCs: pvcs, PVs: pvs, ClaimsInUse: inUse, StatefulSets: sets}, nil
}

func (c *Client) listPVCs(ctx context.Context) ([]PVC, error) {
	var out []PVC
	opts := metav1.ListOptions{Limit: pageSize}
	for {
		page, err := c.core.PersistentVolumeClaims(metav1.NamespaceAll).List(ctx, opts)
		if err != nil {
			return nil, fmt.Errorf("list persistentvolumeclaims: %w", err)
		}
		for i := range page.Items {
			p := &page.Items[i]
			out = append(out, PVC{
				Namespace:     p.Namespace,
				Name:          p.Name,
				UID:           string(p.UID),
				Phase:         string(p.Status.Phase),
				VolumeName:    p.Spec.VolumeName,
				StorageClass:  storageClassOf(p.Spec.StorageClassName),
				CapacityBytes: claimCapacityBytes(p),
				Annotations:   p.Annotations,
			})
		}
		if page.Continue == "" {
			return out, nil
		}
		opts.Continue = page.Continue
	}
}

func (c *Client) listPVs(ctx context.Context) ([]PV, error) {
	var out []PV
	opts := metav1.ListOptions{Limit: pageSize}
	for {
		page, err := c.core.PersistentVolumes().List(ctx, opts)
		if err != nil {
			return nil, fmt.Errorf("list persistentvolumes: %w", err)
		}
		for i := range page.Items {
			p := &page.Items[i]
			v := PV{
				Name:          p.Name,
				UID:           string(p.UID),
				Phase:         string(p.Status.Phase),
				StorageClass:  p.Spec.StorageClassName,
				CapacityBytes: quantityBytes(p.Spec.Capacity),
				ReclaimPolicy: string(p.Spec.PersistentVolumeReclaimPolicy),
				VolumeID:      ebsVolumeID(p),
				Annotations:   p.Annotations,
			}
			if ref := p.Spec.ClaimRef; ref != nil {
				v.ClaimNamespace, v.ClaimName, v.ClaimUID = ref.Namespace, ref.Name, string(ref.UID)
			}
			out = append(out, v)
		}
		if page.Continue == "" {
			return out, nil
		}
		opts.Continue = page.Continue
	}
}

// listClaimsInUse collects "namespace/name" for every claim referenced by a Pod
// that is not in a terminal phase.
//
// Succeeded and Failed Pods are excluded on purpose: their containers are gone
// and mount nothing, but the Pod object survives until something reaps it, so a
// finished Job would otherwise keep its claim looking busy indefinitely.
func (c *Client) listClaimsInUse(ctx context.Context) (map[string]struct{}, error) {
	out := map[string]struct{}{}
	opts := metav1.ListOptions{Limit: pageSize}
	for {
		page, err := c.core.Pods(metav1.NamespaceAll).List(ctx, opts)
		if err != nil {
			return nil, fmt.Errorf("list pods: %w", err)
		}
		for i := range page.Items {
			p := &page.Items[i]
			if p.Status.Phase == corev1.PodSucceeded || p.Status.Phase == corev1.PodFailed {
				continue
			}
			for _, v := range p.Spec.Volumes {
				if v.PersistentVolumeClaim == nil {
					continue
				}
				out[p.Namespace+"/"+v.PersistentVolumeClaim.ClaimName] = struct{}{}
			}
		}
		if page.Continue == "" {
			return out, nil
		}
		opts.Continue = page.Continue
	}
}

func (c *Client) listStatefulSets(ctx context.Context) ([]StatefulSet, error) {
	var out []StatefulSet
	opts := metav1.ListOptions{Limit: pageSize}
	for {
		page, err := c.apps.StatefulSets(metav1.NamespaceAll).List(ctx, opts)
		if err != nil {
			return nil, fmt.Errorf("list statefulsets: %w", err)
		}
		for i := range page.Items {
			out = append(out, newStatefulSet(&page.Items[i]))
		}
		if page.Continue == "" {
			return out, nil
		}
		opts.Continue = page.Continue
	}
}

// newStatefulSet reduces a StatefulSet to the replica range and claim template
// names. spec.replicas is a pointer defaulting to 1, and spec.ordinals is unset
// on everything that has not opted into a non-zero start.
func newStatefulSet(s *appsv1.StatefulSet) StatefulSet {
	out := StatefulSet{Namespace: s.Namespace, Name: s.Name, Replicas: 1}
	if s.Spec.Replicas != nil {
		out.Replicas = *s.Spec.Replicas
	}
	if s.Spec.Ordinals != nil {
		out.OrdinalStart = s.Spec.Ordinals.Start
	}
	for _, t := range s.Spec.VolumeClaimTemplates {
		out.ClaimTemplates = append(out.ClaimTemplates, t.Name)
	}
	return out
}

// AnnotatePVC writes set and deletes remove from a claim's annotations in one
// merge patch, for the same reason the Node client does: concurrent writers (the
// CSI driver, a GitOps controller) never lose their own annotations to a stale
// resourceVersion, and no read-modify-write retry loop is needed.
func (c *Client) AnnotatePVC(ctx context.Context, namespace, name string, set map[string]string, remove []string) error {
	patch, err := annotationPatch(set, remove)
	if err != nil {
		return fmt.Errorf("marshal annotation patch for persistentvolumeclaim %s/%s: %w", namespace, name, err)
	}
	if patch == nil {
		return nil
	}
	if _, err := c.core.PersistentVolumeClaims(namespace).Patch(ctx, name, types.MergePatchType, patch, metav1.PatchOptions{}); err != nil {
		return fmt.Errorf("patch persistentvolumeclaim %s/%s annotations: %w", namespace, name, err)
	}
	return nil
}

// AnnotatePV is AnnotatePVC for the cluster-scoped PersistentVolume.
func (c *Client) AnnotatePV(ctx context.Context, name string, set map[string]string, remove []string) error {
	patch, err := annotationPatch(set, remove)
	if err != nil {
		return fmt.Errorf("marshal annotation patch for persistentvolume %s: %w", name, err)
	}
	if patch == nil {
		return nil
	}
	if _, err := c.core.PersistentVolumes().Patch(ctx, name, types.MergePatchType, patch, metav1.PatchOptions{}); err != nil {
		return fmt.Errorf("patch persistentvolume %s annotations: %w", name, err)
	}
	return nil
}

// annotationPatch builds the merge patch body, or nil when there is nothing to
// write. A merge patch deletes a key by mapping it to JSON null, so the value
// type is *string: a nil pointer encodes as null while a set value encodes as a
// string.
func annotationPatch(set map[string]string, remove []string) ([]byte, error) {
	if len(set) == 0 && len(remove) == 0 {
		return nil, nil
	}
	values := make(map[string]*string, len(set)+len(remove))
	for k, v := range set {
		values[k] = &v
	}
	for _, k := range remove {
		values[k] = nil
	}
	return json.Marshal(map[string]any{
		"metadata": map[string]any{"annotations": values},
	})
}

// ebsVolumeID extracts the EBS volume ID a volume is backed by, empty when it is
// not EBS-backed. The CSI driver stores it verbatim; the removed in-tree plugin
// stored it as aws://<zone>/<volume-id>, and volumes it provisioned outlive the
// plugin itself, which is exactly the population this scanner finds.
func ebsVolumeID(pv *corev1.PersistentVolume) string {
	if csi := pv.Spec.CSI; csi != nil && csi.Driver == ebsCSIDriver {
		return csi.VolumeHandle
	}
	if ebs := pv.Spec.AWSElasticBlockStore; ebs != nil {
		id := ebs.VolumeID
		if i := strings.LastIndex(id, "/"); i >= 0 {
			id = id[i+1:]
		}
		if strings.HasPrefix(id, "vol-") {
			return id
		}
	}
	return ""
}

// storageClassOf resolves the claim's storage class name, which is a pointer
// because an empty string means "no class" while nil means "the default class".
// Neither is a class name, so both report as empty.
func storageClassOf(name *string) string {
	if name == nil {
		return ""
	}
	return *name
}

// claimCapacityBytes prefers the bound capacity over the requested one: a claim
// binds to a volume at least as large as it asked for, and it is the volume that
// bills. The request is the fallback for a claim that never bound.
func claimCapacityBytes(p *corev1.PersistentVolumeClaim) int64 {
	if b := quantityBytes(p.Status.Capacity); b > 0 {
		return b
	}
	return quantityBytes(corev1.ResourceList(p.Spec.Resources.Requests))
}

// quantityBytes reads the storage quantity out of a resource list.
func quantityBytes(list corev1.ResourceList) int64 {
	q, ok := list[corev1.ResourceStorage]
	if !ok {
		return 0
	}
	return q.Value()
}
