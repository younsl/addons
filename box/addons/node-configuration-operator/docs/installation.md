# Installation

## Requirements

- An EKS cluster with managed node groups. Nodes are selected by the `eks.amazonaws.com/nodegroup` label that EKS sets.
- A namespace that admits pods running as root with a hostPath volume. Under Pod Security Admission that is the `privileged` level, which `kube-system` has.
- No IAM role, IRSA, or Pod Identity. The operator calls only the Kubernetes API.

## Install

List the published versions with [crane](https://github.com/google/go-containerregistry/blob/main/cmd/crane/README.md), because OCI charts do not support `helm search repo`.

```bash
crane ls ghcr.io/younsl/charts/node-configuration-operator
```

```bash
helm upgrade --install node-configuration-operator oci://ghcr.io/younsl/charts/node-configuration-operator --namespace kube-system
```

The chart installs the controller Deployment and the agent template in the release namespace. The controller creates the agent DaemonSet once the first NodeConfiguration exists, on the nodes the resources target and on no Karpenter node. Pod settings of the agent are the `agent` chart values.

## Declare resources in the chart

The `nodeConfigurations` value creates the resources with the release, so the operator and its baseline ship together.

```yaml
nodeConfigurations:
  - name: os-hardening
    spec:
      rules:
        - name: restrict-su
          fileMode:
            paths: [/usr/bin/su]
            mode: "4750"
```

Every value is described in the [chart README](../charts/node-configuration-operator/README.md).

## Private registry

The controller and the agent run the same image with the same `imagePullSecrets`, so mirroring one image and setting `image.registry` is enough.

## RBAC

| Subject | Scope | Rules |
| --- | --- | --- |
| Controller | Cluster | Read NodeConfigurations and write their status, manage NodeConfigurationBindings, read Nodes |
| Agent | Cluster | Read NodeConfigurationBindings and write their status |

## Uninstall

Deleting a NodeConfiguration removes its bindings. The nodes keep the configuration that was applied. The CRD stays on `helm uninstall` while `crds.keep` is true.
