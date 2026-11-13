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

The controller and the agent are two images, node-configuration-operator and node-configuration-agent, released together under the same version and pulled with the same `imagePullSecrets`. Mirror both and set `image.registry` and `agent.image.registry`.

## RBAC

| Subject | Scope | Rules |
| --- | --- | --- |
| Controller | Cluster | Read NodeConfigurations and write their status, manage NodeConfigurationBindings, read and patch Nodes, record Events |
| Controller | Release namespace | Read the agent template, manage the agent DaemonSet and, with `agent.podMonitor.enabled`, its PodMonitor, create and update the leader Lease |
| Agent | Cluster | Read NodeConfigurationBindings and write their status |

## High availability

The controller elects a leader through a Lease named after the release in the release namespace. Only the leader rolls configurations out, and the other replicas stay ready and idle.

```yaml
replicaCount: 2
leaderElection:
  enabled: true
  leaseDurationSeconds: 15
  renewDeadlineSeconds: 10
  retryPeriodSeconds: 2
podDisruptionBudget:
  enabled: true
  maxUnavailable: 1
topologySpreadConstraints:
  - maxSkew: 1
    topologyKey: kubernetes.io/hostname
    whenUnsatisfiable: ScheduleAnyway
    labelSelector:
      matchLabels:
        app.kubernetes.io/name: node-configuration-operator
```

- A leader that stops gives the Lease back, so a standby takes over within `retryPeriodSeconds`. After a crash a standby waits `leaseDurationSeconds` on a Lease that is no longer renewed.
- A leader that cannot renew within `renewDeadlineSeconds` stops and restarts as a standby, before any standby may take over.
- Leader election is on by default with one replica too, so the old and the new pod never act together during a rolling update.
- The chart refuses `replicaCount` above 1 with `leaderElection.enabled: false`.
- `topologySpreadConstraints` keeps the replicas on different nodes, and `podDisruptionBudget` keeps a node drain from evicting them all at once. The budget is off by default, because with one replica it would block the drain.
- Watch `node_configuration_controller_leader`, which is 1 on the leader.

```console
$ kubectl get lease node-configuration-operator -n kube-system
NAME                          HOLDER                                        AGE
node-configuration-operator   node-configuration-operator-7d9f6c5b8-x2k4q   3d
```

The agents are a DaemonSet and need no election.

## Uninstall

Deleting a NodeConfiguration makes the controller remove its bindings. A binding is also removed with its node, by the Kubernetes garbage collector. The nodes keep the configuration that was applied. The CRD stays on `helm uninstall` while `crds.keep` is true.

## Conclusion

One chart installs the controller, the agent template, and the RBAC both need, and the NodeConfigurations can ship in the same release. Keep leader election on, add a second replica where the controller must not pause, and remember that an uninstall leaves the nodes as they were configured.
