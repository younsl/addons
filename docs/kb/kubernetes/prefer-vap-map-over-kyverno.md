---
description: Write admission policies as ValidatingAdmissionPolicy and MutatingAdmissionPolicy, using Kyverno only for generate, cleanup, image verification, and background reports.
tags: [kubernetes, admission, policy, cel, kyverno]
resources: [ValidatingAdmissionPolicy, MutatingAdmissionPolicy, ClusterPolicy]
status: adopted
reviewed: 2026-10-10
---

# Prefer VAP and MAP over Kyverno

## Rule

Admission policies default to the built-in ValidatingAdmissionPolicy (VAP) and MutatingAdmissionPolicy (MAP). Kyverno is used only for what the built-in APIs cannot do, listed under Exceptions.

- Validate with VAP, mutate with MAP
- Check MAP availability on the cluster before relying on it: `kubectl api-resources | grep mutatingadmissionpolicies`
- A policy does nothing without a binding, so always ship the policy and its binding together
- Roll out with `validationActions: ["Warn", "Audit"]` first, then switch to `["Deny"]` once no legitimate request is flagged
- `failurePolicy` also covers CEL evaluation errors and misconfigured params, so test expressions against real objects before setting `Fail`

## Why

- **No extra failure domain**: VAP and MAP evaluate CEL inside kube-apiserver. Kyverno runs as an admission webhook, so its availability becomes the API server's availability. With `failurePolicy: Fail`, a crashed or unreachable Kyverno blocks every matched write, including the ones needed to fix Kyverno itself. With `Ignore`, policies are silently skipped during an outage.
- **No network hop**: in-process evaluation adds no webhook round trip, TLS handshake, or timeout to each request. Webhook latency adds up on busy clusters and during large rollouts.
- **Nothing to operate**: no controller Deployments, CRDs, webhook certificates, HA sizing, chart upgrades, or CVE patching. The policy engine upgrades with the control plane.
- **Standard language**: CEL is the same expression language used by CRD validation rules and other Kubernetes APIs, so the skill and the policies carry over to any conformant cluster without vendor lock-in.
- **Native API objects**: policies are plain `admissionregistration.k8s.io` resources that work with GitOps, RBAC, and managed control planes like any other manifest.

## Exceptions

Kyverno is still the right tool for:

- Generating resources (for example a default NetworkPolicy in every new namespace)
- Cleanup policies that delete resources on a schedule or condition
- Image signature and attestation verification (`verifyImages`)
- Background scans and policy reports for resources that already exist, since VAP and MAP act only at admission time
- External data lookups beyond what `paramKind` and `paramRef` can supply

## Example

Reject Deployments whose containers use the `latest` tag.

```yaml
apiVersion: admissionregistration.k8s.io/v1
kind: ValidatingAdmissionPolicy
metadata:
  name: disallow-latest-tag
spec:
  failurePolicy: Fail
  matchConstraints:
    resourceRules:
      - apiGroups: ["apps"]
        apiVersions: ["v1"]
        operations: ["CREATE", "UPDATE"]
        resources: ["deployments"]
  validations:
    - expression: "object.spec.template.spec.containers.all(c, !c.image.endsWith(':latest'))"
      message: "Container images must not use the latest tag."
---
apiVersion: admissionregistration.k8s.io/v1
kind: ValidatingAdmissionPolicyBinding
metadata:
  name: disallow-latest-tag
spec:
  policyName: disallow-latest-tag
  validationActions: ["Deny"]
```

## References

- [Validating Admission Policy](https://kubernetes.io/docs/reference/access-authn-authz/validating-admission-policy/)
- [Mutating Admission Policy](https://kubernetes.io/docs/reference/access-authn-authz/mutating-admission-policy/)
- [CEL in Kubernetes](https://kubernetes.io/docs/reference/using-api/cel/)
