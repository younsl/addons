//! The objects one `Ward` owns, built as plain values so they can be tested
//! without a cluster.

use std::collections::BTreeMap;

use k8s_openapi::api::apps::v1::{DaemonSet, StatefulSet};
use k8s_openapi::api::core::v1::{ConfigMap, Secret, Service, ServiceAccount};
use k8s_openapi::api::rbac::v1::{ClusterRoleBinding, RoleBinding};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference;
use kube::Resource as _;
use serde_json::{Value, json};

use crate::crd::{AgentUpdateStrategy, DnsPolicy, UpdateStrategyType, Ward, WardSpec};
use crate::k8s::{COMPONENT_AGENT, COMPONENT_MANAGER, labels};
use crate::policy::POLICY_FILE;
use crate::rollout::ROLLOUT_FILE;

/// Fixed so the Lease admission policy can recognize agents. Only one
/// `Ward` is active per cluster.
pub const AGENT_SERVICE_ACCOUNT: &str = "ward-agent";
pub const AGENT_PORT: i32 = 8443;
pub const ADMIN_PORT: i32 = 8080;
pub const METRICS_PORT: i32 = 8081;
pub const AGENT_CAPABILITIES: [&str; 9] = [
    "BPF",
    "PERFMON",
    "SYS_RESOURCE",
    "SYS_PTRACE",
    "MAC_ADMIN",
    "KILL",
    "DAC_READ_SEARCH",
    "DAC_OVERRIDE",
    "FOWNER",
];

/// Settings the operator passes down to every `Ward`.
#[derive(Clone, Debug, Default)]
pub struct Defaults {
    pub image: String,
    pub image_pull_policy: String,
    pub workload_cluster_role: String,
    pub lease_cluster_role: String,
    /// The only namespace a `Ward` may live in, the operator's own.
    pub ward_namespace: String,
    /// Prefixes `spec.image` must start with. Empty allows only the
    /// operator's own repository.
    pub allowed_image_prefixes: Vec<String>,
    pub allow_privileged_agent: bool,
}

/// Names of every object a `Ward` owns.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Names {
    pub name: String,
    pub namespace: String,
}

impl Names {
    pub fn of(ward: &Ward) -> Self {
        Self {
            name: ward.metadata.name.clone().unwrap_or_default(),
            namespace: ward.metadata.namespace.clone().unwrap_or_default(),
        }
    }
    pub fn cluster_role_binding(&self) -> String {
        format!("ward-{}-{}", self.namespace, self.name)
    }
    pub fn lease_role_binding(&self) -> String {
        format!("{}-agent-lease", self.name)
    }
    pub fn policy(&self) -> String {
        format!("{}-policy", self.name)
    }
    pub fn rollout(&self) -> String {
        format!("{}-rollout", self.name)
    }
    pub fn agent(&self) -> String {
        format!("{}-agent", self.name)
    }
    pub fn manager(&self) -> String {
        format!("{}-manager", self.name)
    }
    pub fn ca(&self) -> String {
        format!("{}-ca", self.name)
    }
    pub fn manager_tls(&self) -> String {
        format!("{}-manager-tls", self.name)
    }
    pub fn agent_tls(&self) -> String {
        format!("{}-agent-tls", self.name)
    }
    pub fn api_token(&self) -> String {
        format!("{}-api-token", self.name)
    }
    pub fn manager_url(&self) -> String {
        format!(
            "https://{}.{}.svc:{AGENT_PORT}",
            self.manager(),
            self.namespace
        )
    }
    pub fn manager_dns(&self) -> Vec<String> {
        let svc = self.manager();
        let ns = &self.namespace;
        vec![
            svc.clone(),
            format!("{svc}.{ns}"),
            format!("{svc}.{ns}.svc"),
            format!("{svc}.{ns}.svc.cluster.local"),
            format!("*.{svc}.{ns}.svc"),
        ]
    }
}

pub fn owner(ward: &Ward) -> OwnerReference {
    ward.controller_owner_ref(&()).unwrap_or_default()
}

fn meta(ward: &Ward, name: &str, component: &str) -> Value {
    let n = Names::of(ward);
    json!({
        "name": name,
        "namespace": n.namespace,
        "labels": labels(&n.name, component),
        "ownerReferences": [owner(ward)],
    })
}

fn typed<T: serde::de::DeserializeOwned>(v: Value) -> T {
    serde_json::from_value(v).unwrap_or_else(|e| panic!("operator object is well formed: {e}"))
}

fn image(spec: &WardSpec, d: &Defaults) -> (String, String) {
    (
        spec.image.clone().unwrap_or_else(|| d.image.clone()),
        spec.image_pull_policy
            .map_or_else(|| d.image_pull_policy.clone(), |p| p.as_str().to_string()),
    )
}

fn pull_secrets(spec: &WardSpec) -> Value {
    json!(
        spec.image_pull_secrets
            .iter()
            .map(|s| json!({"name": s.name}))
            .collect::<Vec<_>>()
    )
}

pub fn service_account(ward: &Ward) -> ServiceAccount {
    typed(json!({ "metadata": meta(ward, AGENT_SERVICE_ACCOUNT, COMPONENT_AGENT) }))
}

pub fn cluster_role_binding(ward: &Ward, d: &Defaults) -> ClusterRoleBinding {
    let n = Names::of(ward);
    typed(json!({
        "metadata": {
            "name": n.cluster_role_binding(),
            "labels": labels(&n.name, COMPONENT_AGENT),
            "annotations": {"ward.sh/owner": format!("{}/{}", n.namespace, n.name)},
        },
        "roleRef": {"apiGroup": "rbac.authorization.k8s.io", "kind": "ClusterRole", "name": d.workload_cluster_role},
        "subjects": [{"kind": "ServiceAccount", "name": AGENT_SERVICE_ACCOUNT, "namespace": n.namespace}],
    }))
}

pub fn lease_role_binding(ward: &Ward, d: &Defaults) -> RoleBinding {
    let n = Names::of(ward);
    typed(json!({
        "metadata": meta(ward, &n.lease_role_binding(), COMPONENT_AGENT),
        "roleRef": {"apiGroup": "rbac.authorization.k8s.io", "kind": "ClusterRole", "name": d.lease_cluster_role},
        "subjects": [{"kind": "ServiceAccount", "name": AGENT_SERVICE_ACCOUNT, "namespace": n.namespace}],
    }))
}

pub fn config_map(ward: &Ward, name: &str, key: &str, value: &str) -> ConfigMap {
    typed(json!({
        "metadata": meta(ward, name, COMPONENT_AGENT),
        "data": {key: value},
    }))
}

pub fn policy_map(ward: &Ward, policy_json: &str) -> ConfigMap {
    config_map(ward, &Names::of(ward).policy(), POLICY_FILE, policy_json)
}

pub fn rollout_map(ward: &Ward, rollout_json: &str) -> ConfigMap {
    config_map(ward, &Names::of(ward).rollout(), ROLLOUT_FILE, rollout_json)
}

pub fn secret(
    ward: &Ward,
    name: &str,
    data: &BTreeMap<String, String>,
    annotations: &BTreeMap<String, String>,
) -> Secret {
    let mut m = meta(ward, name, COMPONENT_MANAGER);
    m["annotations"] = json!(annotations);
    typed(json!({"metadata": m, "type": "Opaque", "stringData": data}))
}

pub fn manager_service(ward: &Ward) -> Service {
    let n = Names::of(ward);
    typed(json!({
        "metadata": meta(ward, &n.manager(), COMPONENT_MANAGER),
        "spec": {
            "clusterIP": "None",
            "selector": labels(&n.name, COMPONENT_MANAGER).into_iter().filter(|(k, _)| k != crate::k8s::LABEL_MANAGED_BY).collect::<BTreeMap<_, _>>(),
            "ports": [
                {"name": "agent", "port": AGENT_PORT, "targetPort": "agent"},
                {"name": "api", "port": ADMIN_PORT, "targetPort": "api"},
                {"name": "metrics", "port": METRICS_PORT, "targetPort": "metrics"},
            ],
        },
    }))
}

fn selector(n: &Names, component: &str) -> Value {
    json!({"matchLabels": labels(&n.name, component).into_iter().filter(|(k, _)| k != crate::k8s::LABEL_MANAGED_BY).collect::<BTreeMap<_, _>>()})
}

fn probes() -> (Value, Value) {
    (
        json!({"httpGet": {"path": "/readyz", "port": "metrics"}, "periodSeconds": 10}),
        json!({"httpGet": {"path": "/healthz", "port": "metrics"}, "periodSeconds": 20, "failureThreshold": 6}),
    )
}

fn resources(
    r: Option<&k8s_openapi::api::core::v1::ResourceRequirements>,
    cpu: &str,
    mem: &str,
    limit: &str,
) -> Value {
    r.map_or_else(
        || json!({"requests": {"cpu": cpu, "memory": mem}, "limits": {"memory": limit}}),
        |r| serde_json::to_value(r).unwrap_or_default(),
    )
}

pub fn manager_statefulset(ward: &Ward, d: &Defaults) -> StatefulSet {
    let n = Names::of(ward);
    let spec = &ward.spec;
    let (img, pull) = image(spec, d);
    let (ready, live) = probes();
    let mut volumes = vec![
        json!({"name": "policy", "configMap": {"name": n.policy()}}),
        json!({"name": "rollout", "configMap": {"name": n.rollout()}}),
        json!({"name": "tls", "secret": {"secretName": n.manager_tls()}}),
        json!({"name": "token", "secret": {"secretName": n.api_token()}}),
    ];
    let mut mounts = vec![
        json!({"name": "data", "mountPath": "/var/lib/ward"}),
        json!({"name": "policy", "mountPath": "/etc/ward/policy", "readOnly": true}),
        json!({"name": "rollout", "mountPath": "/etc/ward/rollout", "readOnly": true}),
        json!({"name": "tls", "mountPath": "/etc/ward/tls", "readOnly": true}),
        json!({"name": "token", "mountPath": "/etc/ward/api-token", "readOnly": true}),
    ];
    let mut env =
        vec![json!({"name": "WARD_RETENTION", "value": spec.manager.retention.to_string()})];
    if let Some(ps) = spec.feed.as_ref().and_then(|f| f.pull_secret_ref.as_ref()) {
        volumes.push(json!({"name": "registry", "secret": {"secretName": ps.name}}));
        mounts
            .push(json!({"name": "registry", "mountPath": "/etc/ward/registry", "readOnly": true}));
        env.push(
            json!({"name": "WARD_REGISTRY_AUTH", "value": "/etc/ward/registry/.dockerconfigjson"}),
        );
    }
    let mut pvc_labels = labels(&n.name, COMPONENT_MANAGER);
    pvc_labels.remove(crate::k8s::LABEL_MANAGED_BY);
    typed(json!({
        "metadata": meta(ward, &n.manager(), COMPONENT_MANAGER),
        "spec": {
            "serviceName": n.manager(),
            "replicas": 1,
            "selector": selector(&n, COMPONENT_MANAGER),
            "template": {
                "metadata": {"labels": labels(&n.name, COMPONENT_MANAGER)},
                "spec": {
                    "serviceAccountName": AGENT_SERVICE_ACCOUNT,
                    "imagePullSecrets": pull_secrets(spec),
                    "nodeSelector": spec.manager.node_selector,
                    "tolerations": spec.manager.tolerations,
                    "affinity": spec.manager.affinity,
                    "dnsPolicy": spec.manager.dns_policy.map(DnsPolicy::as_str),
                    "dnsConfig": spec.manager.dns_config,
                    "topologySpreadConstraints": spec.manager.topology_spread_constraints,
                    "securityContext": {"runAsNonRoot": true, "runAsUser": 65532, "runAsGroup": 65532, "fsGroup": 65532, "seccompProfile": {"type": "RuntimeDefault"}},
                    "terminationGracePeriodSeconds": 20,
                    "containers": [{
                        "name": "manager",
                        "image": img,
                        "imagePullPolicy": pull,
                        "args": ["manager"],
                        "env": env,
                        "ports": [
                            {"name": "agent", "containerPort": AGENT_PORT},
                            {"name": "api", "containerPort": ADMIN_PORT},
                            {"name": "metrics", "containerPort": METRICS_PORT},
                        ],
                        "readinessProbe": ready,
                        "livenessProbe": live,
                        "resources": resources(spec.manager.resources.as_ref(), "50m", "64Mi", "512Mi"),
                        "securityContext": {"allowPrivilegeEscalation": false, "readOnlyRootFilesystem": true, "capabilities": {"drop": ["ALL"]}},
                        "volumeMounts": mounts,
                    }],
                    "volumes": volumes,
                },
            },
            "volumeClaimTemplates": [{
                "metadata": {"name": "data", "labels": pvc_labels},
                "spec": {
                    "accessModes": ["ReadWriteOnce"],
                    "storageClassName": spec.manager.storage.storage_class_name,
                    "resources": {"requests": {"storage": spec.manager.storage.size}},
                },
            }],
        },
    }))
}

fn update_strategy(s: Option<&AgentUpdateStrategy>) -> Value {
    s.map_or_else(
        || json!({"type": "RollingUpdate", "rollingUpdate": {"maxUnavailable": "10%"}}),
        |s| match (s.kind, &s.rolling_update) {
            (UpdateStrategyType::OnDelete, _) => json!({"type": "OnDelete"}),
            (UpdateStrategyType::RollingUpdate, None) => json!({"type": "RollingUpdate"}),
            (UpdateStrategyType::RollingUpdate, Some(r)) => {
                json!({"type": "RollingUpdate", "rollingUpdate": r})
            }
        },
    )
}

pub fn agent_daemonset(ward: &Ward, d: &Defaults) -> DaemonSet {
    let n = Names::of(ward);
    let spec = &ward.spec;
    let (img, pull) = image(spec, d);
    let (ready, live) = probes();
    let mut env = vec![
        json!({"name": "NODE_NAME", "valueFrom": {"fieldRef": {"fieldPath": "spec.nodeName"}}}),
        json!({"name": "POD_NAMESPACE", "valueFrom": {"fieldRef": {"fieldPath": "metadata.namespace"}}}),
        json!({"name": "WARD_INSTANCE", "value": n.name}),
    ];
    let mut volumes = vec![
        json!({"name": "policy", "configMap": {"name": n.policy()}}),
        json!({"name": "rollout", "configMap": {"name": n.rollout()}}),
        json!({"name": "state", "hostPath": {"path": format!("/var/lib/ward/{}-{}", n.namespace, n.name), "type": "DirectoryOrCreate"}}),
        json!({"name": "tracefs", "hostPath": {"path": "/sys/kernel/tracing"}}),
    ];
    let mut mounts = vec![
        json!({"name": "policy", "mountPath": "/etc/ward/policy", "readOnly": true}),
        json!({"name": "rollout", "mountPath": "/etc/ward/rollout", "readOnly": true}),
        json!({"name": "state", "mountPath": "/var/lib/ward"}),
        json!({"name": "tracefs", "mountPath": "/sys/kernel/tracing", "readOnly": true}),
    ];
    if spec.manager.enabled {
        env.push(json!({"name": "WARD_MANAGER_URL", "value": n.manager_url()}));
        volumes.push(json!({"name": "tls", "secret": {"secretName": n.agent_tls()}}));
        mounts.push(json!({"name": "tls", "mountPath": "/etc/ward/tls", "readOnly": true}));
    } else if let Some(ps) = spec.feed.as_ref().and_then(|f| f.pull_secret_ref.as_ref()) {
        volumes.push(json!({"name": "registry", "secret": {"secretName": ps.name}}));
        mounts
            .push(json!({"name": "registry", "mountPath": "/etc/ward/registry", "readOnly": true}));
        env.push(
            json!({"name": "WARD_REGISTRY_AUTH", "value": "/etc/ward/registry/.dockerconfigjson"}),
        );
    }
    let tolerations = spec
        .agent
        .tolerations
        .clone()
        .map_or_else(|| json!([{"operator": "Exists"}]), |t| json!(t));
    let security = if spec.agent.privileged {
        json!({"privileged": true, "runAsUser": 0, "runAsNonRoot": false, "readOnlyRootFilesystem": true})
    } else {
        json!({
            "runAsUser": 0,
            "runAsNonRoot": false,
            "readOnlyRootFilesystem": true,
            "allowPrivilegeEscalation": false,
            "capabilities": {"drop": ["ALL"], "add": AGENT_CAPABILITIES},
            "seccompProfile": {"type": "Unconfined"},
            "appArmorProfile": {"type": "Unconfined"},
        })
    };
    typed(json!({
        "metadata": meta(ward, &n.agent(), COMPONENT_AGENT),
        "spec": {
            "selector": selector(&n, COMPONENT_AGENT),
            "updateStrategy": update_strategy(spec.agent.update_strategy.as_ref()),
            "template": {
                "metadata": {"labels": labels(&n.name, COMPONENT_AGENT)},
                "spec": {
                    "serviceAccountName": AGENT_SERVICE_ACCOUNT,
                    "hostPID": true,
                    "imagePullSecrets": pull_secrets(spec),
                    "priorityClassName": spec.agent.priority_class_name,
                    "affinity": spec.agent.affinity,
                    "dnsPolicy": spec.agent.dns_policy.map(DnsPolicy::as_str),
                    "dnsConfig": spec.agent.dns_config,
                    "nodeSelector": spec.agent.node_selector,
                    "tolerations": tolerations,
                    "terminationGracePeriodSeconds": 10,
                    "containers": [{
                        "name": "agent",
                        "image": img,
                        "imagePullPolicy": pull,
                        "args": ["agent"],
                        "env": env,
                        "ports": [{"name": "metrics", "containerPort": METRICS_PORT}],
                        "readinessProbe": ready,
                        "livenessProbe": live,
                        "resources": resources(spec.agent.resources.as_ref(), "50m", "128Mi", "512Mi"),
                        "securityContext": security,
                        "volumeMounts": mounts,
                    }],
                    "volumes": volumes,
                },
            },
        },
    }))
}

#[cfg(test)]
pub(crate) mod tests {
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;

    use super::*;
    use crate::crd::{FeedSpec, LocalObjectRef, RolloutSpec};

    pub fn ward(spec: WardSpec) -> Ward {
        Ward {
            metadata: ObjectMeta {
                name: Some("default".into()),
                namespace: Some("ward-system".into()),
                uid: Some("uid-1".into()),
                ..ObjectMeta::default()
            },
            spec,
            status: None,
        }
    }

    pub fn defaults() -> Defaults {
        Defaults {
            image: "ghcr.io/younsl/ward:0.1.0".into(),
            image_pull_policy: "IfNotPresent".into(),
            workload_cluster_role: "ward-workload".into(),
            lease_cluster_role: "ward-agent-lease".into(),
            ward_namespace: "ward-system".into(),
            allowed_image_prefixes: Vec::new(),
            allow_privileged_agent: false,
        }
    }

    #[test]
    fn agent_daemonset_with_and_without_manager() {
        let mut spec = WardSpec::default();
        let ds = serde_json::to_value(agent_daemonset(&ward(spec.clone()), &defaults())).unwrap();
        let pod = &ds["spec"]["template"]["spec"];
        assert_eq!(ds["metadata"]["name"], "default-agent");
        assert_eq!(pod["hostPID"], true);
        assert_eq!(pod["tolerations"][0]["operator"], "Exists");
        let c = &pod["containers"][0];
        assert_eq!(c["ports"][0]["name"], "metrics");
        assert_eq!(
            c["securityContext"]["capabilities"]["add"]
                .as_array()
                .unwrap()
                .len(),
            9
        );
        assert!(
            c["env"]
                .to_string()
                .contains("https://default-manager.ward-system.svc:8443")
        );
        assert_eq!(
            ds["spec"]["template"]["metadata"]["labels"]["app.kubernetes.io/name"],
            "ward"
        );
        assert_eq!(ds["metadata"]["ownerReferences"][0]["kind"], "Ward");

        spec.manager.enabled = false;
        spec.agent.privileged = true;
        spec.image_pull_secrets = vec![LocalObjectRef {
            name: "regcred".into(),
        }];
        spec.feed = Some(FeedSpec {
            image: "ghcr.io/x/ward-db:latest".into(),
            public_key: None,
            insecure_skip_verify: true,
            interval: "6h".into(),
            pull_secret_ref: Some(LocalObjectRef {
                name: "feed-cred".into(),
            }),
            rollout: RolloutSpec::default(),
        });
        let ds = serde_json::to_value(agent_daemonset(&ward(spec), &defaults())).unwrap();
        let pod = &ds["spec"]["template"]["spec"];
        assert_eq!(pod["containers"][0]["securityContext"]["privileged"], true);
        assert!(!pod.to_string().contains("WARD_MANAGER_URL"));
        assert!(pod.to_string().contains("feed-cred"));
        assert_eq!(pod["imagePullSecrets"][0]["name"], "regcred");
        assert_eq!(
            ds["spec"]["updateStrategy"]["rollingUpdate"]["maxUnavailable"],
            "10%"
        );
    }

    #[test]
    fn agent_scheduling_and_update_strategy() {
        use crate::crd::AgentRollingUpdate;
        use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;

        let mut spec = WardSpec::default();
        spec.agent.affinity = Some(serde_json::from_value(serde_json::json!({"nodeAffinity": {"requiredDuringSchedulingIgnoredDuringExecution": {"nodeSelectorTerms": [{"matchExpressions": [{"key": "eks.amazonaws.com/compute-type", "operator": "NotIn", "values": ["fargate"]}]}]}}})).unwrap());
        spec.agent.update_strategy = Some(AgentUpdateStrategy {
            kind: UpdateStrategyType::RollingUpdate,
            rolling_update: Some(AgentRollingUpdate {
                max_unavailable: Some(IntOrString::Int(0)),
                max_surge: Some(IntOrString::String("25%".into())),
            }),
        });
        let ds = serde_json::to_value(agent_daemonset(&ward(spec.clone()), &defaults())).unwrap();
        assert_eq!(
            ds["spec"]["template"]["spec"]["affinity"]["nodeAffinity"]["requiredDuringSchedulingIgnoredDuringExecution"]
                ["nodeSelectorTerms"][0]["matchExpressions"][0]["values"][0],
            "fargate"
        );
        assert_eq!(
            ds["spec"]["updateStrategy"],
            serde_json::json!({"type": "RollingUpdate", "rollingUpdate": {"maxUnavailable": 0, "maxSurge": "25%"}})
        );
        spec.agent.update_strategy = Some(AgentUpdateStrategy {
            kind: UpdateStrategyType::OnDelete,
            rolling_update: None,
        });
        let ds = serde_json::to_value(agent_daemonset(&ward(spec.clone()), &defaults())).unwrap();
        assert_eq!(
            ds["spec"]["updateStrategy"],
            serde_json::json!({"type": "OnDelete"})
        );
        spec.agent.update_strategy = Some(AgentUpdateStrategy::default());
        let ds = serde_json::to_value(agent_daemonset(&ward(spec), &defaults())).unwrap();
        assert_eq!(
            ds["spec"]["updateStrategy"],
            serde_json::json!({"type": "RollingUpdate"})
        );
    }

    #[test]
    fn dns_settings_reach_both_pods() {
        let mut spec = WardSpec::default();
        let dns: k8s_openapi::api::core::v1::PodDNSConfig = serde_json::from_value(
            serde_json::json!({"nameservers": ["10.0.0.10"], "options": [{"name": "ndots", "value": "2"}]}),
        )
        .unwrap();
        spec.manager.dns_policy = Some(DnsPolicy::None);
        spec.manager.dns_config = Some(dns.clone());
        spec.agent.dns_policy = Some(DnsPolicy::ClusterFirst);
        spec.agent.dns_config = Some(dns);
        let w = ward(spec);
        let sts = serde_json::to_value(manager_statefulset(&w, &defaults())).unwrap();
        let ds = serde_json::to_value(agent_daemonset(&w, &defaults())).unwrap();
        assert_eq!(sts["spec"]["template"]["spec"]["dnsPolicy"], "None");
        assert_eq!(
            sts["spec"]["template"]["spec"]["dnsConfig"]["nameservers"][0],
            "10.0.0.10"
        );
        assert_eq!(ds["spec"]["template"]["spec"]["dnsPolicy"], "ClusterFirst");
        assert_eq!(
            ds["spec"]["template"]["spec"]["dnsConfig"]["options"][0]["name"],
            "ndots"
        );
        let plain =
            serde_json::to_value(agent_daemonset(&ward(WardSpec::default()), &defaults())).unwrap();
        assert!(plain["spec"]["template"]["spec"].get("dnsPolicy").is_none());
    }

    #[test]
    fn manager_objects() {
        let mut spec = WardSpec {
            feed: Some(FeedSpec {
                image: "ghcr.io/x/ward-db:latest".into(),
                public_key: None,
                insecure_skip_verify: true,
                interval: "6h".into(),
                pull_secret_ref: Some(LocalObjectRef {
                    name: "feed-cred".into(),
                }),
                rollout: RolloutSpec::default(),
            }),
            ..WardSpec::default()
        };
        spec.manager.node_selector = BTreeMap::from([("pool".into(), "system".into())]);
        spec.manager.tolerations = vec![serde_json::from_value(serde_json::json!({"key": "dedicated", "operator": "Equal", "value": "system", "effect": "NoSchedule"})).unwrap()];
        spec.manager.affinity = Some(serde_json::from_value(serde_json::json!({"nodeAffinity": {"requiredDuringSchedulingIgnoredDuringExecution": {"nodeSelectorTerms": [{"matchExpressions": [{"key": "kubernetes.io/arch", "operator": "In", "values": ["arm64"]}]}]}}})).unwrap());
        spec.manager.topology_spread_constraints = vec![serde_json::from_value(serde_json::json!({"maxSkew": 1, "topologyKey": "topology.kubernetes.io/zone", "whenUnsatisfiable": "ScheduleAnyway", "labelSelector": {"matchLabels": {"app.kubernetes.io/component": "manager"}}})).unwrap()];
        let w = ward(spec);
        let sts = serde_json::to_value(manager_statefulset(&w, &defaults())).unwrap();
        let pod = &sts["spec"]["template"]["spec"];
        assert_eq!(pod["nodeSelector"]["pool"], "system");
        assert_eq!(pod["tolerations"][0]["key"], "dedicated");
        assert_eq!(
            pod["affinity"]["nodeAffinity"]["requiredDuringSchedulingIgnoredDuringExecution"]["nodeSelectorTerms"]
                [0]["matchExpressions"][0]["values"][0],
            "arm64"
        );
        assert_eq!(
            pod["topologySpreadConstraints"][0]["topologyKey"],
            "topology.kubernetes.io/zone"
        );
        assert_eq!(sts["spec"]["serviceName"], "default-manager");
        assert_eq!(
            sts["spec"]["volumeClaimTemplates"][0]["spec"]["resources"]["requests"]["storage"],
            "1Gi"
        );
        assert_eq!(
            sts["spec"]["volumeClaimTemplates"][0]["metadata"]["labels"]["app.kubernetes.io/component"],
            "manager"
        );
        assert!(sts.to_string().contains("WARD_REGISTRY_AUTH"));
        let svc = serde_json::to_value(manager_service(&w)).unwrap();
        assert_eq!(svc["spec"]["clusterIP"], "None");
        assert_eq!(svc["spec"]["ports"].as_array().unwrap().len(), 3);
        let n = Names::of(&w);
        assert_eq!(n.cluster_role_binding(), "ward-ward-system-default");
        assert!(
            n.manager_dns()
                .contains(&"default-manager.ward-system.svc".to_string())
        );
        let crb = serde_json::to_value(cluster_role_binding(&w, &defaults())).unwrap();
        assert_eq!(crb["roleRef"]["name"], "ward-workload");
        assert_eq!(crb["subjects"][0]["namespace"], "ward-system");
        let rb = serde_json::to_value(lease_role_binding(&w, &defaults())).unwrap();
        assert_eq!(rb["roleRef"]["kind"], "ClusterRole");
        let s = secret(
            &w,
            &n.api_token(),
            &BTreeMap::from([("token".into(), "x".into())]),
            &BTreeMap::new(),
        );
        assert_eq!(s.metadata.name.as_deref(), Some("default-api-token"));
        assert_eq!(policy_map(&w, "{}").data.unwrap()[POLICY_FILE], "{}");
        assert_eq!(
            rollout_map(&w, "{}").metadata.name.as_deref(),
            Some("default-rollout")
        );
        assert_eq!(
            service_account(&w).metadata.name.as_deref(),
            Some("ward-agent")
        );
        for name in [n.ca(), n.manager_tls(), n.agent_tls(), n.agent()] {
            assert!(name.starts_with("default-"));
        }
    }
}
